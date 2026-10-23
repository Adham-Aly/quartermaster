use super::*;
use crate::accounts::{self, NewAccount};
use crate::{connection, db::Db, schema};

fn at(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).expect("timestamp")
}

fn model(id: &str) -> ModelId {
    ModelId::new(id).expect("model id")
}

fn memory() -> Connection {
    let mut conn = connection::open_memory().expect("memory database");
    schema::migrate(&mut conn).expect("schema");
    conn
}

fn account(conn: &Connection, label: &str) -> AccountId {
    accounts::insert_account(
        conn,
        &NewAccount {
            provider: Provider::Custom,
            label: label.to_owned(),
            kind: AccountKind::ApiKey,
            credential_ref: None,
            base_url: Some("http://localhost:1234".to_owned()),
            at: at(1),
        },
    )
    .expect("account")
}

fn facts(id: &str, efforts: &[Effort]) -> ModelFacts {
    ModelFacts {
        model: model(id),
        canonical: model(id),
        display_name: None,
        family: id.to_owned(),
        generation: 1,
        image_input: false,
        context_window: 100_000,
        efforts: efforts.to_vec(),
        known: true,
    }
}

fn record(conn: &Connection, account: AccountId, list: &[ModelFacts]) {
    record_model_list(conn, account, ListSource::Account, list, at(2)).expect("models");
}

fn placement(
    conn: &Connection,
    id: i64,
    canonical: &str,
    tier: Tier,
    effort: Option<Effort>,
    seq: i64,
) {
    conn.execute(
        "INSERT INTO placements (id, canonical, tier, effort_override, created_at, seq)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, model(canonical), tier, effort, 1000 - seq, seq],
    )
    .expect("placement");
}

fn migrate(conn: &mut Connection, tiers: &TierEfforts) -> usize {
    let tx = conn.transaction().expect("transaction");
    let resolved = migrate_legacy_efforts(&tx, tiers).expect("migration");
    tx.commit().expect("commit");
    resolved
}

#[test]
fn combos_repeat_across_tiers_and_duplicate_adds_and_effort_changes_are_refused() {
    let mut conn = memory();
    let tx = conn.transaction().expect("transaction");
    let a = account(&tx, "one");
    record(&tx, a, &[facts("reasoner", &[Effort::High, Effort::Low])]);
    let id = model("reasoner");
    for tier in [Tier::Premium, Tier::High, Tier::Medium, Tier::Low] {
        for effort in [Effort::Low, Effort::High] {
            place_model(&tx, &id, None, Some(effort), tier, at(3)).expect("combo");
            assert!(matches!(
                place_model(&tx, &id, None, Some(effort), tier, at(4)),
                Err(ChangeError::Refused(_))
            ));
        }
        assert!(matches!(
            set_placement_effort(&tx, &id, tier, Some(Effort::Low), Some(Effort::High)),
            Err(ChangeError::Refused(_))
        ));
    }
    assert_eq!(list_placements(&tx).expect("placements").len(), 8);
    assert_eq!(pool(&tx).expect("pool").len(), 8);
    // The existing storage combo index is the backstop, not a new schema.
    assert!(
        tx.execute(
            "INSERT INTO placements (id, canonical, tier, effort_override, created_at, seq)
         VALUES (999, 'reasoner', 'high', 'high', 9, 999)",
            [],
        )
        .is_err()
    );
    // Old callers may still store None as a tier-default entry.
    place_model(&tx, &id, None, None, Tier::High, at(5)).expect("legacy caller");
    assert_eq!(list_placements(&tx).expect("placements").len(), 9);
    tx.commit().expect("commit");
}

#[test]
fn stale_moves_after_migration_and_duplicate_destinations_preserve_sources() {
    let mut conn = memory();
    let a = account(&conn, "one");
    record(&conn, a, &[facts("reasoner", &[])]);
    let id = model("reasoner");
    placement(&conn, 1, "reasoner", Tier::High, None, 1);
    // A move prepared from this legacy snapshot races the capability-resolution pass.
    record(&conn, a, &[facts("reasoner", &[Effort::High])]);
    assert_eq!(migrate(&mut conn, &TierEfforts::default()), 1);
    let resolved = list_placements(&conn).expect("resolved");
    assert!(matches!(
        place_model(&conn, &id, Some(Tier::High), None, Tier::Medium, at(3)),
        Err(ChangeError::Refused(_))
    ));
    assert_eq!(list_placements(&conn).expect("unchanged"), resolved);
    assert!(matches!(
        remove_placement(&conn, &id, Tier::High, None, at(3)),
        Err(ChangeError::Refused(_))
    ));
    place_model(&conn, &id, None, Some(Effort::High), Tier::Medium, at(4)).expect("second tier");
    let both = list_placements(&conn).expect("both entries");
    assert!(matches!(
        place_model(
            &conn,
            &id,
            Some(Tier::High),
            Some(Effort::High),
            Tier::Medium,
            at(5)
        ),
        Err(ChangeError::Refused(_))
    ));
    assert_eq!(list_placements(&conn).expect("source retained"), both);
    place_model(
        &conn,
        &id,
        Some(Tier::High),
        Some(Effort::High),
        Tier::Low,
        at(6),
    )
    .expect("fresh move");
    let moved = list_placements(&conn).expect("moved");
    assert_eq!(moved.len(), 2);
    assert!(
        moved
            .iter()
            .all(|p| p.effort_override == Some(Effort::High))
    );
    assert!(!moved.iter().any(|p| p.tier == Tier::High));
}

#[test]
fn no_effort_models_keep_one_none_combo_per_tier() {
    let mut conn = memory();
    let a = account(&conn, "one");
    record(&conn, a, &[facts("plain", &[])]);
    let id = model("plain");
    for tier in [Tier::Premium, Tier::High, Tier::Medium, Tier::Low] {
        place_model(&conn, &id, None, None, tier, at(3)).expect("no effort combo");
        assert!(matches!(
            place_model(&conn, &id, None, None, tier, at(4)),
            Err(ChangeError::Refused(_))
        ));
    }
    let before = list_placements(&conn).expect("placements");
    assert_eq!(before.len(), 4);
    assert!(before.iter().all(|p| p.effort_override.is_none()));
    assert_eq!(migrate(&mut conn, &TierEfforts::default()), 0);
    assert_eq!(list_placements(&conn).expect("preserved"), before);
}

#[test]
fn migration_uses_custom_tiers_caps_to_supported_and_preserves_explicit_efforts() {
    let mut conn = memory();
    let a = account(&conn, "one");
    record(
        &conn,
        a,
        &[facts("reasoner", &[Effort::High, Effort::Medium])],
    );
    for (id, tier) in [
        (1, Tier::Premium),
        (2, Tier::High),
        (3, Tier::Medium),
        (4, Tier::Low),
    ] {
        placement(&conn, id, "reasoner", tier, None, id);
    }
    placement(&conn, 5, "reasoner", Tier::High, Some(Effort::Max), 5);
    let tiers = TierEfforts {
        premium: Effort::Low,
        high: Effort::Max,
        medium: Effort::High,
        low: Effort::ExtraHigh,
    };
    assert_eq!(migrate(&mut conn, &tiers), 4);
    let rows = list_placements(&conn).expect("placements");
    assert_eq!(
        rows.iter().map(|p| p.effort_override).collect::<Vec<_>>(),
        vec![
            Some(Effort::Medium),
            Some(Effort::High),
            Some(Effort::High),
            Some(Effort::High),
            Some(Effort::Max),
        ]
    );
    assert_eq!(rows[0].created_at, at(999));
    assert_eq!(migrate(&mut conn, &TierEfforts::default()), 0);
    assert_eq!(list_placements(&conn).expect("unchanged"), rows);
}

#[test]
fn collisions_keep_earliest_seq_not_id_or_creation_time() {
    let mut conn = memory();
    let a = account(&conn, "one");
    record(&conn, a, &[facts("reasoner", &[Effort::High])]);
    // Legacy first: it keeps its id/seq/time, replacing the later explicit collision only.
    placement(&conn, 90, "reasoner", Tier::High, None, 10);
    placement(&conn, 1, "reasoner", Tier::High, Some(Effort::High), 20);
    // Explicit first: the later legacy entry is removed.
    placement(&conn, 80, "reasoner", Tier::Low, Some(Effort::High), 5);
    placement(&conn, 2, "reasoner", Tier::Low, None, 30);
    // Another tier independently merges its collision; seq is unique in the schema.
    placement(&conn, 3, "reasoner", Tier::Premium, None, 40);
    placement(&conn, 70, "reasoner", Tier::Premium, Some(Effort::High), 41);
    placement(&conn, 4, "reasoner", Tier::Medium, None, 50);
    placement(&conn, 5, "reasoner", Tier::Medium, Some(Effort::Low), 60);
    assert_eq!(migrate(&mut conn, &TierEfforts::default()), 4);
    let retained = conn
        .prepare("SELECT id, seq, created_at FROM placements ORDER BY seq, id")
        .expect("query")
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .expect("rows")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("retained");
    assert_eq!(
        retained,
        vec![
            (80, 5, 995),
            (90, 10, 990),
            (3, 40, 960),
            (4, 50, 950),
            (5, 60, 940)
        ]
    );
    assert_eq!(migrate(&mut conn, &TierEfforts::default()), 0);
}

#[test]
fn unavailable_and_no_effort_placements_survive_and_can_resolve_later() {
    let mut conn = memory();
    let a = account(&conn, "one");
    let removed = account(&conn, "removed");
    record(
        &conn,
        a,
        &[facts("plain", &[]), facts("gone", &[Effort::High])],
    );
    record(&conn, a, &[facts("plain", &[])]);
    record(&conn, removed, &[facts("removed", &[Effort::High])]);
    accounts::disconnect(&conn, removed, at(3)).expect("disconnect");
    for (id, name) in [(1, "plain"), (2, "gone"), (3, "unknown"), (4, "removed")] {
        placement(&conn, id, name, Tier::High, None, id);
    }
    let before = list_placements(&conn).expect("placements");
    assert_eq!(migrate(&mut conn, &TierEfforts::default()), 0);
    assert_eq!(list_placements(&conn).expect("preserved"), before);
    record(
        &conn,
        a,
        &[
            facts("plain", &[]),
            facts("gone", &[Effort::High]),
            facts("unknown", &[Effort::Medium]),
        ],
    );
    assert_eq!(migrate(&mut conn, &TierEfforts::default()), 2);
    // An unknown/no-effort catalog entry can acquire capabilities without losing its placement.
    record(&conn, a, &[facts("plain", &[Effort::Low])]);
    assert_eq!(migrate(&mut conn, &TierEfforts::default()), 1);
    let after = list_placements(&conn).expect("placements");
    assert_eq!(after.len(), 4);
    assert_eq!(after[0].effort_override, Some(Effort::Low));
    assert_eq!(after[3].effort_override, None);
}

#[test]
fn capability_union_is_sorted_deduplicated_and_shared_with_migration_and_writers() {
    let mut conn = memory();
    let a = account(&conn, "one");
    let b = account(&conn, "two");
    record(
        &conn,
        a,
        &[facts(
            "reasoner",
            &[Effort::High, Effort::Low, Effort::High],
        )],
    );
    let mut alias = facts("alias", &[Effort::Max, Effort::Medium, Effort::Low]);
    alias.canonical = model("reasoner");
    record(&conn, b, &[alias]);
    accounts::set_paused(&conn, b, true).expect("pause");
    let expected = vec![Effort::Low, Effort::Medium, Effort::High, Effort::Max];
    assert_eq!(
        supported_efforts(&conn).expect("capabilities")[&model("reasoner")],
        expected
    );
    assert_eq!(
        offered_model(&conn, &model("reasoner")).expect("offered").2,
        expected
    );
    placement(&conn, 1, "reasoner", Tier::High, None, 1);
    let tiers = TierEfforts {
        high: Effort::Max,
        ..TierEfforts::default()
    };
    assert_eq!(migrate(&mut conn, &tiers), 1);
    assert_eq!(
        list_placements(&conn).expect("placement")[0].effort_override,
        Some(Effort::Max)
    );
    place_model(
        &conn,
        &model("reasoner"),
        None,
        Some(Effort::Medium),
        Tier::Low,
        at(5),
    )
    .expect("union effort accepted");
}

#[tokio::test]
async fn migrated_combos_and_unresolved_entries_survive_restart() {
    let dir = std::env::temp_dir().join(format!("qm-model-efforts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch folder");
    let path = dir.join("state.db");
    let db = Db::open(&path).expect("database");
    let before = db
        .call(|conn| {
            let tx = conn.transaction()?;
            let a = account(&tx, "one");
            record(&tx, a, &[facts("reasoner", &[Effort::Low, Effort::High])]);
            placement(&tx, 1, "reasoner", Tier::High, None, 1);
            placement(&tx, 2, "reasoner", Tier::High, Some(Effort::Low), 2);
            placement(&tx, 3, "reasoner", Tier::Low, Some(Effort::High), 3);
            placement(&tx, 4, "unknown", Tier::Medium, None, 4);
            placement(&tx, 5, "reasoner", Tier::High, Some(Effort::High), 5);
            assert_eq!(migrate_legacy_efforts(&tx, &TierEfforts::default())?, 1);
            assert_eq!(list_placements(&tx)?.len(), 4);
            tx.commit()?;
            list_placements(conn)
        })
        .await
        .expect("seed and migrate");
    drop(db);
    let db = Db::open(&path).expect("restart");
    let after = db
        .call(|conn| {
            let tx = conn.transaction()?;
            let tiers = TierEfforts {
                high: Effort::Low,
                ..TierEfforts::default()
            };
            assert_eq!(migrate_legacy_efforts(&tx, &tiers)?, 0);
            tx.commit()?;
            list_placements(conn)
        })
        .await
        .expect("migration after restart");
    assert_eq!(after, before);
    drop(db);
    std::fs::remove_dir_all(&dir).expect("cleanup");
}
