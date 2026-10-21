//! Detected models and tier placements.
//!
//! - **Detected models** are rows per (account, model): canonical id, family, capabilities, effort
//!   levels, first seen, new, gone. [`record_model_list`] reconciles an account's latest list
//!   with what's stored.
//! - **New models are never used automatically.** A model that newly appears is marked new and
//!   starts "not in use": [`pool`] leaves it out until a placement made after it appeared puts it
//!   in a tier. The first list read for an account isn't "new" (the starter set covers it), nor is
//!   a model you already have through another account.
//! - **Announcements happen once.** A model's appearance and disappearance are each announced by
//!   one `events` row (`model.new`, `model.gone`; the announcement line is the row's `summary`),
//!   written at the moment of the change in the caller's transaction, so a repeat read never
//!   announces again.
//! - **Gone models keep their placement** in case they return; they just drop out of the pool.
//! - **Starter tiers are seeded once per provider** ([`seed_starter_placements`]): an
//!   `events` row (`starter_tiers_seeded`) records it, and the inserts never touch an existing
//!   placement, so your later changes are never overwritten.
//!
//! `models.capabilities` holds `{image_input, context_window, generation, known, source}`: `known =
//! false` means the catalog didn't know the model and cautious defaults apply; `source` is
//! `account` for a list the account reported and `catalog` for the catalog's list used when nothing
//! could be read.
//!
//! - **Your changes on the tier board** ([`place_model`], [`remove_placement`],
//!   [`take_out_of_use`], [`set_placement_effort`], [`list_model`]) refuse what doesn't apply
//!   with a plain [`ChangeError::Refused`]. Placing a model clears its "new" mark. A model found
//!   with find, or left in no tier by your change, is listed under NOT IN USE for good: a
//!   `settings.model_listed` event records it ([`listed_models`]).
//!
//! Like every function in this crate, these run inside the caller's transaction where they write
//! more than one row ([`record_model_list`], [`seed_starter_placements`], [`place_model`],
//! [`remove_placement`], [`take_out_of_use`]).

use crate::StoreError;
use crate::accounts::ChangeError;
use crate::time::{from_millis, to_millis};
use jiff::Timestamp;
use qm_types::domain::{AccountKind, Effort, Provider, Tier};
use qm_types::ids::{AccountId, ModelId};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Where a model list came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListSource {
    /// The account's own list.
    Account,
    /// The built-in catalog, used when the account's list couldn't be read and none was recorded
    /// before.
    Catalog,
}

/// What's known about one detected model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFacts {
    /// The id the account uses.
    pub model: ModelId,
    /// The id placements key on.
    pub canonical: ModelId,
    pub display_name: Option<String>,
    pub family: String,
    pub generation: u32,
    pub image_input: bool,
    pub context_window: u64,
    pub efforts: Vec<Effort>,
    /// The catalog knows it; `false` means cautious defaults.
    pub known: bool,
}

#[derive(Serialize, Deserialize)]
struct Capabilities {
    image_input: bool,
    context_window: u64,
    generation: u32,
    known: bool,
    source: ListSource,
}

/// One stored model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    pub account: AccountId,
    pub facts: ModelFacts,
    pub source: ListSource,
    pub first_seen_at: Timestamp,
    pub is_new: bool,
    pub gone_at: Option<Timestamp>,
}

/// What [`record_model_list`] changed and announced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    /// This was the account's first list.
    pub first_list: bool,
    /// Canonical ids announced as new.
    pub announced_new: Vec<ModelId>,
    /// Canonical ids announced as gone.
    pub announced_gone: Vec<ModelId>,
    /// Account model ids that came back after being gone.
    pub returned: Vec<ModelId>,
}

fn to_json<T: Serialize>(column: &'static str, v: &T) -> Result<String, StoreError> {
    serde_json::to_string(v).map_err(|source| StoreError::Json { column, source })
}

fn from_json<T: for<'de> Deserialize<'de>>(column: &'static str, s: &str) -> Result<T, StoreError> {
    serde_json::from_str(s).map_err(|source| StoreError::Json { column, source })
}

/// Every model stored for an account, gone ones included, in first-seen order.
pub fn list_models(conn: &Connection, account: AccountId) -> Result<Vec<ModelRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT model, canonical, family, display_name, capabilities, efforts, first_seen_at,
                is_new, gone_at
         FROM models WHERE account_id = ?1 ORDER BY id",
    )?;
    let raw = stmt
        .query_map(params![account], |r| {
            Ok((
                r.get::<_, ModelId>(0)?,
                r.get::<_, ModelId>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
                r.get::<_, bool>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(
            |(model, canonical, family, display_name, caps, efforts, first, is_new, gone)| {
                let caps: Capabilities = from_json("models.capabilities", &caps)?;
                Ok(ModelRow {
                    account,
                    facts: ModelFacts {
                        family: family.unwrap_or_else(|| canonical.as_str().to_owned()),
                        model,
                        canonical,
                        display_name,
                        generation: caps.generation,
                        image_input: caps.image_input,
                        context_window: caps.context_window,
                        efforts: from_json("models.efforts", &efforts)?,
                        known: caps.known,
                    },
                    source: caps.source,
                    first_seen_at: from_millis("models.first_seen_at", first)?,
                    is_new,
                    gone_at: gone
                        .map(|ms| from_millis("models.gone_at", ms))
                        .transpose()?,
                })
            },
        )
        .collect()
}

/// Another active account already offers this canonical model (not gone).
fn offered_elsewhere(
    conn: &Connection,
    canonical: &ModelId,
    account: AccountId,
) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM models AS m JOIN accounts AS a ON a.id = m.account_id
             WHERE m.canonical = ?1 AND m.account_id <> ?2
               AND m.gone_at IS NULL AND a.removed_at IS NULL)",
        params![canonical, account],
        |r| r.get(0),
    )?)
}

fn announce(
    conn: &Connection,
    kind: &str,
    summary: &str,
    account: AccountId,
    facts: &ModelFacts,
    at: Timestamp,
) -> Result<(), StoreError> {
    let payload = serde_json::json!({
        "account": account,
        "model": facts.model,
        "canonical": facts.canonical,
        "known": facts.known,
    });
    conn.execute(
        "INSERT INTO events (at, kind, summary, payload) VALUES (?1, ?2, ?3, ?4)",
        params![
            to_millis(at),
            kind,
            summary,
            to_json("events.payload", &payload)?
        ],
    )?;
    Ok(())
}

fn label(facts: &ModelFacts) -> String {
    match &facts.display_name {
        Some(name) => format!("{name} ({})", facts.model),
        None => facts.model.to_string(),
    }
}

/// Reconciles an account's latest model list with what's stored, announcing each appearance and
/// disappearance once. Call inside one transaction.
///
/// - A list from the catalog (`ListSource::Catalog`) is recorded only as a stand-in: it announces
///   nothing, marks nothing gone, and its rows are quietly retired by the first real list.
/// - On the first list for an account nothing is new.
pub fn record_model_list(
    conn: &Connection,
    account: AccountId,
    source: ListSource,
    list: &[ModelFacts],
    at: Timestamp,
) -> Result<Recorded, StoreError> {
    let existing = list_models(conn, account)?;
    let mut out = Recorded {
        first_list: existing.is_empty(),
        ..Recorded::default()
    };
    let at_ms = to_millis(at);

    for facts in list {
        let caps = to_json(
            "models.capabilities",
            &Capabilities {
                image_input: facts.image_input,
                context_window: facts.context_window,
                generation: facts.generation,
                known: facts.known,
                source,
            },
        )?;
        let efforts = to_json("models.efforts", &facts.efforts)?;
        if let Some(row) = existing.iter().find(|r| r.facts.model == facts.model) {
            conn.execute(
                "UPDATE models SET canonical = ?3, family = ?4, display_name = ?5,
                         capabilities = ?6, efforts = ?7, gone_at = NULL
                     WHERE account_id = ?1 AND model = ?2",
                params![
                    account,
                    facts.model,
                    facts.canonical,
                    facts.family,
                    facts.display_name,
                    caps,
                    efforts
                ],
            )?;
            if row.gone_at.is_some() {
                out.returned.push(facts.model.clone());
            }
        } else {
            let is_new = source == ListSource::Account
                && !out.first_list
                && !offered_elsewhere(conn, &facts.canonical, account)?;
            conn.execute(
                "INSERT INTO models (account_id, model, canonical, family, display_name,
                         capabilities, efforts, first_seen_at, is_new)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    account,
                    facts.model,
                    facts.canonical,
                    facts.family,
                    facts.display_name,
                    caps,
                    efforts,
                    at_ms,
                    is_new
                ],
            )?;
            if is_new {
                announce(
                    conn,
                    "model.new",
                    &format!(
                        "New model available: {}. It isn't used until you place it in a tier",
                        label(facts)
                    ),
                    account,
                    facts,
                    at,
                )?;
                out.announced_new.push(facts.canonical.clone());
            }
        }
    }

    if source == ListSource::Account {
        mark_gone(conn, account, &existing, list, at, &mut out)?;
    }
    Ok(out)
}

/// Marks models absent from a real list as gone, announcing each disappearance once.
fn mark_gone(
    conn: &Connection,
    account: AccountId,
    existing: &[ModelRow],
    list: &[ModelFacts],
    at: Timestamp,
    out: &mut Recorded,
) -> Result<(), StoreError> {
    let at_ms = to_millis(at);
    for row in existing.iter().filter(|r| r.gone_at.is_none()) {
        if list.iter().any(|f| f.model == row.facts.model) {
            continue;
        }
        conn.execute(
            "UPDATE models SET gone_at = ?3 WHERE account_id = ?1 AND model = ?2",
            params![account, row.facts.model, at_ms],
        )?;
        // A catalog stand-in the account never offered retires quietly; a model still
        // offered through another account hasn't disappeared from your pool.
        if row.source == ListSource::Account
            && !offered_elsewhere(conn, &row.facts.canonical, account)?
        {
            announce(
                conn,
                "model.gone",
                &format!(
                    "Model no longer offered: {}. It keeps its tier placement in case it returns",
                    label(&row.facts)
                ),
                account,
                &row.facts,
                at,
            )?;
            out.announced_gone.push(row.facts.canonical.clone());
        }
    }
    Ok(())
}

/// Clears a model's "new" mark once you've seen it.
pub fn mark_seen(conn: &Connection, account: AccountId, model: &ModelId) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE models SET is_new = 0 WHERE account_id = ?1 AND model = ?2",
        params![account, model],
    )?;
    Ok(())
}

/// One tier entry: a model at an effort in a tier. A model may sit in one tier more than once,
/// at different efforts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlacementRow {
    pub canonical: ModelId,
    pub tier: Tier,
    pub effort_override: Option<Effort>,
    pub created_at: Timestamp,
}

/// Every placement, in the order they were made.
pub fn list_placements(conn: &Connection) -> Result<Vec<PlacementRow>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT canonical, tier, effort_override, created_at FROM placements ORDER BY id",
    )?;
    let raw = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, ModelId>(0)?,
                r.get::<_, Tier>(1)?,
                r.get::<_, Option<Effort>>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(|(canonical, tier, effort_override, at)| {
            Ok(PlacementRow {
                canonical,
                tier,
                effort_override,
                created_at: from_millis("placements.created_at", at)?,
            })
        })
        .collect()
}

const SEEDED: &str = "starter_tiers_seeded";

/// The provider's starter set was seeded before.
pub fn starter_seeded(conn: &Connection, provider: Provider) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM events
                        WHERE kind = ?1 AND json_extract(payload, '$.provider') = ?2)",
        params![SEEDED, provider.as_str()],
        |r| r.get(0),
    )?)
}

/// Seeds a provider's starter set once: does nothing if it was seeded before, and never changes
/// an existing placement. Returns how many placements it added. Call inside one transaction.
pub fn seed_starter_placements(
    conn: &Connection,
    provider: Provider,
    approved: bool,
    placements: &[(Tier, ModelId)],
    at: Timestamp,
) -> Result<usize, StoreError> {
    if starter_seeded(conn, provider)? {
        return Ok(0);
    }
    let mut added = 0;
    for (tier, canonical) in placements {
        // A model already in the tier, at any effort, is left as you set it.
        added += conn.execute(
            "INSERT INTO placements (canonical, tier, created_at)
             SELECT ?1, ?2, ?3
             WHERE NOT EXISTS (SELECT 1 FROM placements WHERE canonical = ?1 AND tier = ?2)",
            params![canonical, tier, to_millis(at)],
        )?;
    }
    let payload = serde_json::json!({
        "provider": provider.as_str(),
        "approved": approved,
        "placements": placements
            .iter()
            .map(|(t, m)| serde_json::json!({ "tier": t.as_str(), "model": m }))
            .collect::<Vec<_>>(),
    });
    conn.execute(
        "INSERT INTO events (at, kind, summary, payload) VALUES (?1, ?2, ?3, ?4)",
        params![
            to_millis(at),
            SEEDED,
            format!("Starter tiers set up for {provider}"),
            to_json("events.payload", &payload)?
        ],
    )?;
    Ok(added)
}

/// One (model, account) pair routing can pick from: a stored model on an active, unpaused account
/// that isn't gone, joined with a placement when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolRow {
    pub account: AccountId,
    pub provider: Provider,
    pub account_kind: AccountKind,
    /// `accounts.usage` (JSON of `qm_engine::UsageReading`).
    pub usage: Option<String>,
    pub facts: ModelFacts,
    /// `None` for a pair outside any tier (see [`offered`]).
    pub tier: Option<Tier>,
    pub effort_override: Option<Effort>,
}

const POOL_SELECT: &str = "SELECT a.id, a.provider, a.kind, a.usage,
        m.model, m.canonical, m.family, m.display_name, m.capabilities, m.efforts";

fn pool_rows(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<PoolRow>, StoreError> {
    let mut stmt = conn.prepare(sql)?;
    let raw = stmt
        .query_map(args, |r| {
            Ok((
                (
                    r.get::<_, AccountId>(0)?,
                    r.get::<_, Provider>(1)?,
                    r.get::<_, AccountKind>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ),
                (
                    r.get::<_, ModelId>(4)?,
                    r.get::<_, ModelId>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?,
                ),
                (
                    r.get::<_, Option<Tier>>(10)?,
                    r.get::<_, Option<Effort>>(11)?,
                ),
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(
            |(
                (account, provider, account_kind, usage),
                (model, canonical, family, display_name, caps, efforts),
                (tier, effort_override),
            )| {
                let caps: Capabilities = from_json("models.capabilities", &caps)?;
                Ok(PoolRow {
                    account,
                    provider,
                    account_kind,
                    usage,
                    facts: ModelFacts {
                        family: family.unwrap_or_else(|| canonical.as_str().to_owned()),
                        model,
                        canonical,
                        display_name,
                        generation: caps.generation,
                        image_input: caps.image_input,
                        context_window: caps.context_window,
                        efforts: from_json("models.efforts", &efforts)?,
                        known: caps.known,
                    },
                    tier,
                    effort_override,
                })
            },
        )
        .collect()
}

/// Every placed (model, account) pair routing may use, in placement order. Leaves out paused or
/// removed accounts, gone models, and a new model until a placement made after it appeared puts
/// it in a tier ("it isn't routed until you place it in a tier").
pub fn pool(conn: &Connection) -> Result<Vec<PoolRow>, StoreError> {
    pool_rows(
        conn,
        &format!(
            "{POOL_SELECT}, p.tier, p.effort_override
             FROM placements AS p
             JOIN models AS m ON m.canonical = p.canonical
             JOIN accounts AS a ON a.id = m.account_id
             WHERE a.removed_at IS NULL AND a.paused = 0 AND m.gone_at IS NULL
               AND NOT (m.is_new = 1 AND p.created_at < m.first_seen_at)
             ORDER BY p.id, a.id"
        ),
        [],
    )
}

/// Every pair offering a canonical model, placed or not (for the "always use" pin and its
/// fallback, which ignore tiers, and for the Quartermaster's named model). Same exclusions
/// as [`pool`] except placement.
pub fn offered(conn: &Connection, canonical: &ModelId) -> Result<Vec<PoolRow>, StoreError> {
    pool_rows(
        conn,
        &format!(
            "{POOL_SELECT}, NULL, NULL
             FROM models AS m
             JOIN accounts AS a ON a.id = m.account_id
             WHERE m.canonical = ?1
               AND a.removed_at IS NULL AND a.paused = 0 AND m.gone_at IS NULL
             ORDER BY a.id"
        ),
        params![canonical],
    )
}

/// The canonical id for a model id an account uses, if any account offers it under that id.
pub fn canonical_of(conn: &Connection, model: &ModelId) -> Result<Option<ModelId>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT canonical FROM models WHERE model = ?1 ORDER BY id LIMIT 1",
            params![model],
            |r| r.get(0),
        )
        .optional()?)
}

// ---- your changes on the tier board ----
//
// Placements key on canonical ids; each writer takes the canonical id the Settings screens show
// (an account's own id for the model is accepted too). Placing a model by hand clears its "new"
// mark, so the pool routes it from then on. These never touch the starter-tier record, so a
// provider is still seeded at most once and seeding still never overwrites what you set.

/// `events.kind` marking a model listed under NOT IN USE (no section prefix: it's the Settings
/// screens' own bookkeeping and never reaches the context block).
const LISTED: &str = "settings.model_listed";

/// A model an active account offers (not gone), by canonical id or an account's own id: its
/// canonical id, display name and the effort levels any account offers it at.
fn offered_model(
    conn: &Connection,
    model: &ModelId,
) -> Result<(ModelId, String, Vec<Effort>), ChangeError> {
    let mut stmt = conn.prepare(
        "SELECT m.canonical, m.display_name, m.efforts
         FROM models AS m JOIN accounts AS a ON a.id = m.account_id
         WHERE (m.canonical = ?1 OR m.model = ?1)
           AND m.gone_at IS NULL AND a.removed_at IS NULL
         ORDER BY (m.canonical = ?1) DESC, m.id",
    )?;
    let rows = stmt
        .query_map(params![model], |r| {
            Ok((
                r.get::<_, ModelId>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some((canonical, name, _)) = rows.first().cloned() else {
        return Err(ChangeError::Refused(format!(
            "No connected account offers {model}; nothing was changed."
        )));
    };
    let mut efforts: Vec<Effort> = Vec::new();
    for (c, _, json) in &rows {
        if *c != canonical {
            continue;
        }
        for e in from_json::<Vec<Effort>>("models.efforts", json)? {
            if !efforts.contains(&e) {
                efforts.push(e);
            }
        }
    }
    let name = name.unwrap_or_else(|| canonical.to_string());
    Ok((canonical, name, efforts))
}

/// The canonical id for a model a writer names, whether or not anything offers it now (a gone
/// model keeps its placements, and you can still take it out of a tier).
fn canonical_for(conn: &Connection, model: &ModelId) -> Result<ModelId, StoreError> {
    let known: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM placements WHERE canonical = ?1)
             OR EXISTS (SELECT 1 FROM models WHERE canonical = ?1)",
        params![model],
        |r| r.get(0),
    )?;
    if known {
        return Ok(model.clone());
    }
    Ok(canonical_of(conn, model)?.unwrap_or_else(|| model.clone()))
}

/// How a refusal names a tier entry's effort: "High effort", or "the tier default".
fn entry_effort(effort: Option<Effort>) -> String {
    effort.map_or_else(
        || "the tier default".to_owned(),
        |e| format!("{} effort", e.label()),
    )
}

/// Whether `tier` holds `canonical` at `effort` (`None` is the tier default).
fn has_entry(
    conn: &Connection,
    canonical: &ModelId,
    tier: Tier,
    effort: Option<Effort>,
) -> Result<bool, StoreError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM placements
                        WHERE canonical = ?1 AND tier = ?2 AND effort_override IS ?3)",
        params![canonical, tier, effort],
        |r| r.get(0),
    )?)
}

/// Refuses an effort the model doesn't offer (a model that reports no levels takes any).
fn check_offered(
    name: &str,
    efforts: &[Effort],
    effort: Option<Effort>,
) -> Result<(), ChangeError> {
    match effort {
        Some(e) if !efforts.is_empty() && !efforts.contains(&e) => Err(ChangeError::Refused(
            format!("{name} doesn't offer {e} effort; nothing was changed."),
        )),
        _ => Ok(()),
    }
}

/// Puts a tier entry (a model at an effort; `None` is the tier default) in tier `to`: moved out
/// of `from`, or added when `from` is `None`. The same model may sit in one tier more than once
/// at different efforts; adding an entry the tier already holds is refused, and moving onto one
/// just merges the two. Clears the model's "new" mark, so the pool routes it. Call inside one
/// transaction.
pub fn place_model(
    conn: &Connection,
    model: &ModelId,
    from: Option<Tier>,
    effort: Option<Effort>,
    to: Tier,
    at: Timestamp,
) -> Result<(), ChangeError> {
    let (canonical, name, efforts) = offered_model(conn, model)?;
    match from {
        Some(from) if from == to => return Ok(()),
        Some(from) => {
            conn.execute(
                "DELETE FROM placements
                 WHERE canonical = ?1 AND tier = ?2 AND effort_override IS ?3",
                params![canonical, from, effort],
            )?;
        }
        None => {
            check_offered(&name, &efforts, effort)?;
            if has_entry(conn, &canonical, to, effort)? {
                return Err(ChangeError::Refused(format!(
                    "{name} is already in the {to} tier at {}; nothing was changed.",
                    entry_effort(effort)
                )));
            }
        }
    }
    if !has_entry(conn, &canonical, to, effort)? {
        conn.execute(
            "INSERT INTO placements (canonical, tier, effort_override, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![canonical, to, effort, to_millis(at)],
        )?;
    }
    conn.execute(
        "UPDATE models SET is_new = 0 WHERE canonical = ?1",
        params![canonical],
    )?;
    Ok(())
}

/// Takes one tier entry (the model at `effort` in `tier`) out. A model left in no tier is listed
/// under NOT IN USE, where you put it down. Call inside one transaction.
pub fn remove_placement(
    conn: &Connection,
    model: &ModelId,
    tier: Tier,
    effort: Option<Effort>,
    at: Timestamp,
) -> Result<(), ChangeError> {
    let canonical = canonical_for(conn, model)?;
    let removed = conn.execute(
        "DELETE FROM placements WHERE canonical = ?1 AND tier = ?2 AND effort_override IS ?3",
        params![canonical, tier, effort],
    )?;
    if removed == 0 {
        return Err(ChangeError::Refused(format!(
            "{canonical} isn't in the {tier} tier at {}; nothing was changed.",
            entry_effort(effort)
        )));
    }
    let left: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM placements WHERE canonical = ?1)",
        params![canonical],
        |r| r.get(0),
    )?;
    if !left {
        record_listed(conn, &canonical, at)?;
    }
    Ok(())
}

/// Takes a model out of every tier, so it isn't routed, and lists it under NOT IN USE. Call
/// inside one transaction.
pub fn take_out_of_use(
    conn: &Connection,
    model: &ModelId,
    at: Timestamp,
) -> Result<(), ChangeError> {
    let canonical = canonical_for(conn, model)?;
    conn.execute(
        "DELETE FROM placements WHERE canonical = ?1",
        params![canonical],
    )?;
    record_listed(conn, &canonical, at)?;
    Ok(())
}

/// Changes one tier entry's effort (`effort_override`) from `current` to `effort`; `None` is the
/// tier default. An effort the model doesn't offer is refused, and so is one the tier already
/// holds the model at.
pub fn set_placement_effort(
    conn: &Connection,
    model: &ModelId,
    tier: Tier,
    current: Option<Effort>,
    effort: Option<Effort>,
) -> Result<(), ChangeError> {
    let (canonical, name, efforts) = offered_model(conn, model)?;
    check_offered(&name, &efforts, effort)?;
    if !has_entry(conn, &canonical, tier, current)? {
        return Err(ChangeError::Refused(format!(
            "{name} isn't in the {tier} tier at {}; nothing was changed.",
            entry_effort(current)
        )));
    }
    if current == effort {
        return Ok(());
    }
    if has_entry(conn, &canonical, tier, effort)? {
        return Err(ChangeError::Refused(format!(
            "{name} is already in the {tier} tier at {}; nothing was changed.",
            entry_effort(effort)
        )));
    }
    conn.execute(
        "UPDATE placements SET effort_override = ?4
         WHERE canonical = ?1 AND tier = ?2 AND effort_override IS ?3",
        params![canonical, tier, current, effort],
    )?;
    Ok(())
}

/// Lists a model found with find under NOT IN USE, for good (it survives a relaunch).
pub fn list_model(conn: &Connection, model: &ModelId, at: Timestamp) -> Result<(), ChangeError> {
    let (canonical, _, _) = offered_model(conn, model)?;
    record_listed(conn, &canonical, at)?;
    Ok(())
}

/// Records a model as listed under NOT IN USE, once.
fn record_listed(conn: &Connection, canonical: &ModelId, at: Timestamp) -> Result<(), StoreError> {
    if listed_models(conn)?.contains(canonical) {
        return Ok(());
    }
    let payload = serde_json::json!({ "model": canonical });
    conn.execute(
        "INSERT INTO events (at, kind, summary, payload) VALUES (?1, ?2, ?3, ?4)",
        params![
            to_millis(at),
            LISTED,
            format!("{canonical} listed under not in use"),
            to_json("events.payload", &payload)?
        ],
    )?;
    Ok(())
}

/// The canonical ids listed under NOT IN USE: found with find, or left in no tier by your change.
/// A listing is never taken back (events are append-only); the screens show it only while the
/// model is in no tier.
pub fn listed_models(conn: &Connection) -> Result<BTreeSet<ModelId>, StoreError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT json_extract(payload, '$.model') FROM events
         WHERE kind = ?1 AND json_extract(payload, '$.model') IS NOT NULL",
    )?;
    let ids = stmt
        .query_map(params![LISTED], |r| r.get::<_, ModelId>(0))?
        .collect::<rusqlite::Result<BTreeSet<_>>>()?;
    Ok(ids)
}
