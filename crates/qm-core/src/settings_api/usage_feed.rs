//! Keeps `accounts.usage` current for quota-aware routing: every minute, each active account's
//! usage windows as its engine last saw them (`Engine::usage`: Claude's `rate_limit_event`
//! windows, a provider's usage headers). Both engines answer from memory, so this never starts a
//! process or makes a request. A reading is written only when it changed.

use super::{Inner, accounts};
use crate::routing;
use std::sync::Arc;
use std::time::Duration;

const PERIOD: Duration = Duration::from_secs(60);

pub(super) fn start(inner: &Arc<Inner>) {
    let weak = Arc::downgrade(inner);
    let clock = Arc::clone(&inner.clock);
    tokio::spawn(async move {
        loop {
            let Some(inner) = weak.upgrade() else { break };
            feed(&inner).await;
            drop(inner);
            clock.sleep(PERIOD).await;
        }
    });
}

/// One pass over the active accounts.
pub(super) async fn feed(inner: &Arc<Inner>) {
    let Ok(rows) = inner.db.call(|conn| accounts::list_active(conn)).await else {
        return;
    };
    let mut changed = false;
    for row in rows {
        let Some(engine) = inner.engines.get(&row.engine) else {
            continue;
        };
        let Ok(Some(reading)) = engine.usage(&routing::account_ref(&row)).await else {
            continue;
        };
        let Ok(json) = serde_json::to_string(&reading) else {
            continue;
        };
        if row.usage.as_deref() == Some(json.as_str()) {
            continue;
        }
        let (id, at) = (row.id, reading.read_at);
        match inner
            .db
            .call(move |conn| accounts::record_usage(conn, id, &json, at))
            .await
        {
            Ok(()) => changed = true,
            Err(e) => tracing::warn!(error = %e, "couldn't store the account's usage reading"),
        }
    }
    if changed {
        inner.changed();
    }
}
