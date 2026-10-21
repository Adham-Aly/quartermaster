//! Connected accounts.
//!
//! One row per account: provider, label, kind (subscription or key), the engine it runs on (which
//! follows the provider), paused, preferred, the last usage reading and the key of its
//! credentials in `auth.json` (never the secret). Accounts are never deleted: removing one sets
//! `removed_at`.
//!
//! Reconnecting an account whose login broke keeps its row: the new credential goes in under its
//! `credential_ref` ([`set_credential_ref`] only if it had none), so its ID, label, placements and
//! preferred mark stay.
//!
//! Your changes in Settings ([`set_label`], [`set_paused`], [`disconnect`], [`set_preferred`])
//! respect the schema's unique indexes (active labels are unique, at most one account is
//! preferred) and refuse what doesn't apply with a plain [`ChangeError::Refused`], never a panic.

use crate::StoreError;
use crate::seq;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{AccountKind, EngineKind, Provider};
use qm_types::ids::AccountId;
use rusqlite::{Connection, OptionalExtension, Row, params};

/// A new account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewAccount {
    pub provider: Provider,
    pub label: String,
    pub kind: AccountKind,
    /// The key of its credentials in `auth.json`; `None` for a Claude subscription.
    pub credential_ref: Option<String>,
    pub base_url: Option<String>,
    pub at: Timestamp,
}

/// One account as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountRow {
    pub id: AccountId,
    pub provider: Provider,
    pub label: String,
    pub kind: AccountKind,
    pub engine: EngineKind,
    pub paused: bool,
    pub preferred: bool,
    /// The last usage reading, as JSON (`qm_engine::UsageReading`), if any.
    pub usage: Option<String>,
    pub usage_read_at: Option<Timestamp>,
    pub credential_ref: Option<String>,
    pub base_url: Option<String>,
    pub created_at: Timestamp,
    pub removed_at: Option<Timestamp>,
}

const COLUMNS: &str = "id, provider, label, kind, engine, paused, preferred, usage, usage_read_at, \
                       credential_ref, base_url, created_at, removed_at";

fn row_to_account(r: &Row<'_>) -> rusqlite::Result<RawAccount> {
    Ok(RawAccount {
        id: r.get(0)?,
        provider: r.get(1)?,
        label: r.get(2)?,
        kind: r.get(3)?,
        engine: r.get(4)?,
        paused: r.get(5)?,
        preferred: r.get(6)?,
        usage: r.get(7)?,
        usage_read_at: r.get(8)?,
        credential_ref: r.get(9)?,
        base_url: r.get(10)?,
        created_at: r.get(11)?,
        removed_at: r.get(12)?,
    })
}

struct RawAccount {
    id: AccountId,
    provider: Provider,
    label: String,
    kind: AccountKind,
    engine: EngineKind,
    paused: bool,
    preferred: bool,
    usage: Option<String>,
    usage_read_at: Option<i64>,
    credential_ref: Option<String>,
    base_url: Option<String>,
    created_at: i64,
    removed_at: Option<i64>,
}

impl RawAccount {
    fn finish(self) -> Result<AccountRow, StoreError> {
        Ok(AccountRow {
            id: self.id,
            provider: self.provider,
            label: self.label,
            kind: self.kind,
            engine: self.engine,
            paused: self.paused,
            preferred: self.preferred,
            usage: self.usage,
            usage_read_at: self
                .usage_read_at
                .map(|ms| from_millis("accounts.usage_read_at", ms))
                .transpose()?,
            credential_ref: self.credential_ref,
            base_url: self.base_url,
            created_at: from_millis("accounts.created_at", self.created_at)?,
            removed_at: self
                .removed_at
                .map(|ms| from_millis("accounts.removed_at", ms))
                .transpose()?,
        })
    }
}

/// Records a new account. Its engine follows its provider.
pub fn insert_account(conn: &Connection, new: &NewAccount) -> Result<AccountId, StoreError> {
    let id = conn.query_row(
        "INSERT INTO accounts (provider, label, kind, engine, credential_ref, base_url, created_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING id",
        params![
            new.provider,
            new.label,
            new.kind,
            new.provider.engine(),
            new.credential_ref,
            new.base_url,
            to_millis(new.at),
            seq::next(new.at),
        ],
        |r| r.get(0),
    )?;
    Ok(id)
}

/// One account, removed or not.
pub fn load_account(conn: &Connection, id: AccountId) -> Result<Option<AccountRow>, StoreError> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM accounts WHERE id = ?1"),
        params![id],
        row_to_account,
    )
    .optional()?
    .map(RawAccount::finish)
    .transpose()
}

/// Every account not removed, oldest first.
pub fn list_active(conn: &Connection) -> Result<Vec<AccountRow>, StoreError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM accounts WHERE removed_at IS NULL ORDER BY seq"
    ))?;
    let raw = stmt
        .query_map([], row_to_account)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter().map(RawAccount::finish).collect()
}

/// Your preferred account for crew work, if one is set.
pub fn preferred_account(conn: &Connection) -> Result<Option<AccountId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT id FROM accounts WHERE preferred = 1 AND removed_at IS NULL",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

/// Stores an account's latest usage reading (JSON of `qm_engine::UsageReading`), read at most every
/// few minutes.
pub fn record_usage(
    conn: &Connection,
    id: AccountId,
    usage_json: &str,
    at: Timestamp,
) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE accounts SET usage = ?2, usage_read_at = ?3 WHERE id = ?1",
        params![id, usage_json, to_millis(at)],
    )?;
    Ok(())
}

/// Why a change you asked for in Settings wasn't made.
#[derive(Debug, thiserror::Error)]
pub enum ChangeError {
    /// The change doesn't apply (an unknown account, a label already taken, …); the sentence is
    /// plain enough to show as it is.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<rusqlite::Error> for ChangeError {
    fn from(e: rusqlite::Error) -> Self {
        ChangeError::Store(e.into())
    }
}

/// An account that isn't removed, or a refusal naming it.
fn active_account(conn: &Connection, id: AccountId) -> Result<AccountRow, ChangeError> {
    match load_account(conn, id)? {
        Some(row) if row.removed_at.is_none() => Ok(row),
        _ => Err(ChangeError::Refused(format!(
            "There's no connected account {id}."
        ))),
    }
}

/// Renames an account. Active labels are unique: a label another active account has is refused.
pub fn set_label(conn: &Connection, id: AccountId, label: &str) -> Result<(), ChangeError> {
    let row = active_account(conn, id)?;
    let label = label.trim();
    if label.is_empty() {
        return Err(ChangeError::Refused(
            "An account's label can't be empty.".into(),
        ));
    }
    if label == row.label {
        return Ok(());
    }
    let taken: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM accounts
                        WHERE label = ?1 AND id <> ?2 AND removed_at IS NULL)",
        params![label, id],
        |r| r.get(0),
    )?;
    if taken {
        return Err(ChangeError::Refused(format!(
            "Another account is already called “{label}”; pick a different label."
        )));
    }
    conn.execute(
        "UPDATE accounts SET label = ?2 WHERE id = ?1",
        params![id, label],
    )?;
    Ok(())
}

/// Points an account at a new credential key in `auth.json`, for reconnecting an account that had
/// none. Its ID, label, placements and preferred mark are untouched.
pub fn set_credential_ref(
    conn: &Connection,
    id: AccountId,
    credential_ref: &str,
) -> Result<(), ChangeError> {
    active_account(conn, id)?;
    conn.execute(
        "UPDATE accounts SET credential_ref = ?2 WHERE id = ?1",
        params![id, credential_ref],
    )?;
    Ok(())
}

/// Pauses or resumes an account. A paused account is never routed (`models::pool` and
/// `models::offered` leave it out).
pub fn set_paused(conn: &Connection, id: AccountId, paused: bool) -> Result<(), ChangeError> {
    active_account(conn, id)?;
    conn.execute(
        "UPDATE accounts SET paused = ?2 WHERE id = ?1",
        params![id, paused],
    )?;
    Ok(())
}

/// Disconnects an account: sets `removed_at` (accounts are never deleted) and drops its preferred
/// mark, so it leaves routing, the pool and the Settings screens. Its placements stay: they key on
/// canonical models, which other accounts may offer. Returns the row as it was, so the caller can
/// forget its credentials (`credential_ref`).
pub fn disconnect(
    conn: &Connection,
    id: AccountId,
    at: Timestamp,
) -> Result<AccountRow, ChangeError> {
    let row = active_account(conn, id)?;
    conn.execute(
        "UPDATE accounts SET removed_at = ?2, preferred = 0 WHERE id = ?1",
        params![id, to_millis(at)],
    )?;
    Ok(row)
}

/// Makes `id` your one preferred account for crew work, or clears the mark when `None`. The old
/// preferred account loses its mark first, so the "at most one" index never clashes. Call inside
/// one transaction.
pub fn set_preferred(conn: &Connection, id: Option<AccountId>) -> Result<(), ChangeError> {
    if let Some(id) = id {
        active_account(conn, id)?;
    }
    conn.execute("UPDATE accounts SET preferred = 0 WHERE preferred = 1", [])?;
    if let Some(id) = id {
        conn.execute(
            "UPDATE accounts SET preferred = 1 WHERE id = ?1",
            params![id],
        )?;
    }
    Ok(())
}
