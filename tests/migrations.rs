use fridica::{
    config,
    store::{migration, schema, Store},
};
use rusqlite::Connection;
use std::path::Path;

fn v5(path: &Path) -> Connection {
    let c = Connection::open(path).unwrap();
    for (n, sql) in schema::MIGRATIONS[..5].iter().enumerate() {
        c.execute_batch(sql).unwrap();
        c.execute(
            "INSERT OR REPLACE INTO meta VALUES('schema_version',?)",
            [(n + 1).to_string()],
        )
        .unwrap();
    }
    c
}
fn config_file(path: &Path) {
    std::fs::write(path, "# owner settings\n[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[machines.box]\nhost='box'\n[machines.box.workspaces]\nwork='/work'\n[state]\npath='state.sqlite3'\n[limits]\nmax_wait_replies = 5 # patience\nmax_no_progress = 3\nmax_jobs = 4\n").unwrap();
}

#[test]
fn configuration_preserves_comments_and_explicit_override() {
    let source =
        "# Hello\n[limits]\nmax_wait_replies = 6 # slow\nmax_no_progress = 2\nmax_jobs = 4\n";
    let edited = config::migrate_text(source).unwrap();
    assert!(
        edited.contains("# Hello") && edited.contains("# slow") && edited.contains("max_jobs = 4")
    );
    assert!(edited.contains("streak_signal = 2"));
    assert_eq!(edited, config::migrate_text(&edited).unwrap());
    let explicit = format!("{source}\n[attention]\nstreak_signal = 9 # override\n");
    let edited = config::migrate_text(&explicit).unwrap();
    assert!(edited.contains("streak_signal = 9 # override"));
    for key in ["max_wait_replies", "max_no_progress"] {
        let edited = config::migrate_text(&format!("[limits]\n{key} = 9\n")).unwrap();
        assert!(edited.contains("streak_signal = 3"));
    }
}

#[test]
fn migration_is_additive_idempotent_and_conservative_about_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let cfg = dir.path().join("config.toml");
    config_file(&cfg);
    let c = v5(&db);
    for (id, reason) in [
        (
            "auto",
            "3 turns without progress; review before continuing.",
        ),
        ("owner", "Paused by the owner."),
        ("unknown", "possible loop"),
    ] {
        c.execute("INSERT INTO threads(id,workspace,channel,root_ts,control,pause_reason,created,updated) VALUES(?,'W','C',?,'paused',?,1,1)", [id,id,reason]).unwrap();
    }
    drop(c);
    let original = std::fs::read(&cfg).unwrap();
    let plan = fridica::cli::migrate::dry_run(&db, &cfg).unwrap();
    assert_eq!(plan.from, 5);
    assert_eq!(plan.automatic_pauses, 1);
    assert_eq!(std::fs::read(&cfg).unwrap(), original);
    fridica::cli::migrate::migrate(&db, &cfg, 10.).unwrap();
    fridica::cli::migrate::migrate(&db, &cfg, 11.).unwrap();
    let c = Connection::open(&db).unwrap();
    assert_eq!(schema::version(&c).unwrap(), schema::VERSION);
    let paused: i64 = c
        .query_row(
            "SELECT count(*) FROM threads WHERE control='paused'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(paused, 2);
    let audits: i64 = c
        .query_row(
            "SELECT count(*) FROM audit WHERE action='migration.resume'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(audits, 1);
    assert_eq!(
        c.query_row("SELECT count(*) FROM obligations", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(c);
    fridica::cli::migrate::rollback(&db, &cfg).unwrap();
    assert_eq!(schema::version(&Connection::open(&db).unwrap()).unwrap(), 5);
    assert_eq!(std::fs::read(&cfg).unwrap(), original);
}

#[test]
fn rollback_refuses_mutations_without_audit_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let cfg = dir.path().join("config.toml");
    drop(v5(&db));
    config_file(&cfg);
    fridica::cli::migrate::migrate(&db, &cfg, 1.).unwrap();
    let c = Connection::open(&db).unwrap();
    c.execute(
        "INSERT INTO health_events(kind,details_json,created) VALUES('disconnect','{}',2)",
        [],
    )
    .unwrap();
    c.execute("DELETE FROM health_events", []).unwrap();
    assert!(fridica::cli::migrate::rollback(&db, &cfg)
        .unwrap_err()
        .to_string()
        .contains("durable mutations"));
}

#[tokio::test]
async fn fresh_store_bounds_and_serializes_transactions_and_excludes_second_writer() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let store = Store::open(db.clone()).await.unwrap();
    assert!(Store::open(db).await.is_err());
    let mut tasks = Vec::new();
    for i in 0..200 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {store.call(move |c| {
            let tx=c.transaction()?;
            tx.execute("INSERT INTO health_events(kind,details_json,created) VALUES('test','{}',?)",[i])?;
            tx.commit()?; Ok(())
        }).await.unwrap()}));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(
        store
            .call(
                |c| Ok(c.query_row("SELECT count(*) FROM health_events", [], |r| r
                    .get::<_, i64>(0))?)
            )
            .await
            .unwrap(),
        200
    );
}

#[test]
fn interrupted_config_export_resumes_without_duplicate_events() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let cfg = dir.path().join("config.toml");
    drop(v5(&db));
    config_file(&cfg);
    let original = std::fs::read(&cfg).unwrap();
    fridica::cli::migrate::migrate(&db, &cfg, 1.).unwrap();
    let path = dir.path().join("state.sqlite3.migration.json");
    let mut journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    journal["phase"] = "prepared".into();
    journal["generation"] = serde_json::Value::Null;
    std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
    std::fs::write(&cfg, original).unwrap();
    assert!(migration::check_ready(&db).is_err());
    fridica::cli::migrate::migrate(&db, &cfg, 2.).unwrap();
    migration::check_ready(&db).unwrap();
    fridica::cli::migrate::rollback(&db, &cfg).unwrap();
}

#[test]
fn migration_preflight_rejects_invalid_policy_and_mismatched_database_without_backups() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let cfg = dir.path().join("config.toml");
    config_file(&cfg);
    drop(v5(&db));
    let original = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(
        &cfg,
        format!("{original}\n[policy]\nfetch_repos=['owner/repo']\n"),
    )
    .unwrap();
    assert!(fridica::cli::migrate::migrate(&db, &cfg, 1.)
        .unwrap_err()
        .to_string()
        .contains("fetch_repos"));
    assert_eq!(schema::version(&Connection::open(&db).unwrap()).unwrap(), 5);
    let suffix = migration::backup_suffix();
    assert!(!dir.path().join(format!("state.sqlite3{suffix}")).exists());
    assert!(!dir.path().join(format!("config.toml{suffix}")).exists());
    assert!(!dir.path().join("state.sqlite3.migration.json").exists());
    std::fs::write(&cfg, original.replace("state.sqlite3", "other.sqlite3")).unwrap();
    assert!(fridica::cli::migrate::dry_run(&db, &cfg)
        .unwrap_err()
        .to_string()
        .contains("differs"));
}

/// A database at `version` (6 or later) that came from v5 and has been used
/// since: the conversion marker and its baseline are there, and the mutation
/// guards have counted every write after it (#86).
fn used_at(path: &Path, version: usize) -> Connection {
    let c = Connection::open(path).unwrap();
    for (n, sql) in schema::MIGRATIONS[..version].iter().enumerate() {
        c.execute_batch(sql).unwrap();
        c.execute(
            "INSERT OR REPLACE INTO meta VALUES('schema_version',?)",
            [(n + 1).to_string()],
        )
        .unwrap();
    }
    c.execute_batch(
        "INSERT INTO meta VALUES('v6_converted','1'); INSERT INTO meta VALUES('v6_migration_generation','1');",
    )
    .unwrap();
    schema::install_mutation_guards(&c).unwrap();
    for n in 0..5 {
        c.execute("INSERT INTO threads(id,workspace,channel,root_ts,created,updated) VALUES(?,'W','C',?,1,1)", [format!("t{n}"), n.to_string()]).unwrap();
    }
    let generation: i64 = c
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='durable_generation'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(generation > 1, "{generation}");
    c
}
#[tokio::test]
async fn a_used_v6_database_upgrades_and_rolls_back_cleanly() {
    used_database_upgrades_and_rolls_back(6).await;
}
#[tokio::test]
async fn a_used_v7_database_upgrades_and_rolls_back_cleanly() {
    used_database_upgrades_and_rolls_back(7).await;
}
#[tokio::test]
async fn a_used_v8_database_upgrades_and_rolls_back_cleanly() {
    used_database_upgrades_and_rolls_back(8).await;
}
async fn used_database_upgrades_and_rolls_back(version: usize) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let cfg = dir.path().join("config.toml");
    config_file(&cfg);
    drop(used_at(&db, version));
    let plan = fridica::cli::migrate::dry_run(&db, &cfg).unwrap();
    assert_eq!((plan.from, plan.to), (version, schema::VERSION));
    fridica::cli::migrate::migrate(&db, &cfg, 10.).unwrap();
    fridica::cli::migrate::migrate(&db, &cfg, 11.).unwrap();
    let journal: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("state.sqlite3.migration.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(journal["phase"], "complete");
    {
        let c = Connection::open(&db).unwrap();
        assert_eq!(schema::version(&c).unwrap(), schema::VERSION);
        let columns: Vec<String> = c
            .prepare("SELECT name FROM pragma_table_info('jobs')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(columns.iter().any(|c| c == "files_json"));
        assert!(columns.iter().any(|c| c == "snapshot_json"));
        assert!(columns.iter().any(|c| c == "fork_from_worker"));
        // Jobs from before worker forks read back with no source.
        let sources: i64 = c
            .query_row(
                "SELECT count(*) FROM jobs WHERE fork_from_worker!=''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sources, 0);
        // The v5 -> v6 baseline is history, not this upgrade's.
        let baseline: String = c
            .query_row(
                "SELECT value FROM meta WHERE key='v6_migration_generation'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(baseline, "1");
    }
    // The daemon's store accepts the result; closing it releases the lock
    // before the rollback takes it (dropping alone would race the thread).
    let store = Store::open(db.clone()).await.unwrap();
    store.close().await.unwrap();
    assert!(store.call(|_| Ok(())).await.is_err());
    fridica::cli::migrate::rollback(&db, &cfg).unwrap();
    assert_eq!(
        schema::version(&Connection::open(&db).unwrap()).unwrap(),
        version
    );
}
#[test]
fn rollback_after_a_used_v6_upgrade_refuses_once_the_daemon_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let cfg = dir.path().join("config.toml");
    config_file(&cfg);
    drop(used_at(&db, 6));
    fridica::cli::migrate::migrate(&db, &cfg, 10.).unwrap();
    Connection::open(&db)
        .unwrap()
        .execute("INSERT INTO meta VALUES('post_migration_write','1')", [])
        .unwrap();
    let error = fridica::cli::migrate::rollback(&db, &cfg)
        .unwrap_err()
        .to_string();
    assert!(error.contains("durable mutations"), "{error}");
}

/// The next upgrade after a finished one (#106): the earlier upgrade's
/// `complete` journal is archived beside its backup instead of blocking the
/// new upgrade as "not resumable", and the new upgrade still rolls back.
#[tokio::test]
async fn a_completed_journal_from_an_earlier_upgrade_does_not_block_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let cfg = dir.path().join("config.toml");
    config_file(&cfg);
    drop(used_at(&db, 7));
    // What the v4 -> v7 upgrade left behind.
    let journal = dir.path().join("state.sqlite3.migration.json");
    let earlier = serde_json::json!({
        "database": std::fs::canonicalize(&db).unwrap(),
        "config": std::fs::canonicalize(&cfg).unwrap(),
        "from": 4,
        "config_before": "earlier",
        "config_after": "earlier",
        "backup_hash": "earlier",
        "phase": "complete",
        "generation": 1
    });
    std::fs::write(&journal, serde_json::to_vec(&earlier).unwrap()).unwrap();
    fridica::cli::migrate::migrate(&db, &cfg, 10.).unwrap();
    assert_eq!(
        schema::version(&Connection::open(&db).unwrap()).unwrap(),
        schema::VERSION
    );
    let archived: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("state.sqlite3.migration.v7.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(archived, earlier);
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
    assert_eq!(
        (current["phase"].as_str(), current["from"].as_u64()),
        (Some("complete"), Some(7))
    );
    fridica::cli::migrate::rollback(&db, &cfg).unwrap();
    assert_eq!(schema::version(&Connection::open(&db).unwrap()).unwrap(), 7);

    // An archive in the way is preserved, never overwritten.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.sqlite3");
    let cfg = dir.path().join("config.toml");
    config_file(&cfg);
    drop(used_at(&db, 7));
    let mut earlier = earlier;
    earlier["database"] = serde_json::json!(std::fs::canonicalize(&db).unwrap());
    earlier["config"] = serde_json::json!(std::fs::canonicalize(&cfg).unwrap());
    std::fs::write(
        dir.path().join("state.sqlite3.migration.json"),
        serde_json::to_vec(&earlier).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.path().join("state.sqlite3.migration.v7.json"), b"keep").unwrap();
    assert!(fridica::cli::migrate::migrate(&db, &cfg, 10.).is_err());
    assert_eq!(
        std::fs::read(dir.path().join("state.sqlite3.migration.v7.json")).unwrap(),
        b"keep"
    );
    assert_eq!(schema::version(&Connection::open(&db).unwrap()).unwrap(), 7);
}
#[tokio::test]
async fn a_used_v9_database_upgrades_and_rolls_back_cleanly() {
    used_database_upgrades_and_rolls_back(9).await;
}
/// A database from before the channel ledger links its last week of messages
/// once (#108): references and thread pointers, within one channel.
#[test]
fn the_ledger_backfills_recent_messages_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Connection::open(dir.path().join("db")).unwrap();
    schema::migrate(&mut c).unwrap();
    for (event, ts, root, text, at) in [
        (
            "e1",
            "1790927185.684379",
            "1790927185.684379",
            "Review chengcli/snapy#269",
            100.,
        ),
        (
            "e2",
            "1790944278.167119",
            "1790944278.167119",
            "SIGN-OFF #269 in thread 1790927185.684379",
            200.,
        ),
        (
            "e3",
            "1790000000.000001",
            "1790000000.000001",
            "Old: #269",
            1.,
        ),
    ] {
        c.execute("INSERT OR IGNORE INTO threads(id,workspace,channel,root_ts,created,updated) VALUES(?,'W','C',?,1,1)", [format!("W:C:{root}"), root.to_string()]).unwrap();
        c.execute("INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,files_json,source,received_at) VALUES(?,'W','C',?,?,'U',?,'[]','socket',?)",
            rusqlite::params![event, ts, root, text, at]).unwrap();
    }
    // A top-level post of Fridica's own (a debrief) has no thread row.
    c.execute("INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,files_json,source,received_at) VALUES('e4','W','C','1790950000.000001','1790950000.000001','U','Debrief: #269 done','[]','self',150)", []).unwrap();
    let now = 100. + 86400.;
    assert_eq!(
        fridica::store::links::backfill_tx(&c, now, 86400.).unwrap(),
        3
    );
    assert_eq!(
        fridica::store::links::backfill_tx(&c, now, 86400.).unwrap(),
        0
    );
    let items: Vec<(String, String)> = c
        .prepare("SELECT session_id,repo FROM item_links WHERE item='#269' ORDER BY session_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(
        items,
        [
            ("W:C:1790927185.684379".to_string(), "snapy".to_string()),
            ("W:C:1790944278.167119".to_string(), String::new())
        ]
    );
    let target: String = c
        .query_row(
            "SELECT target FROM thread_links WHERE session_id='W:C:1790944278.167119'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(target, "W:C:1790927185.684379");
}

/// The per-pass checks for pending controls, worker stops and configuration
/// edits find their rows by index, not by scanning replay_events, which only
/// grows. A hand-made index of the same name does not break the upgrade.
#[test]
fn per_pass_replay_lookups_use_the_kind_index() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Connection::open(dir.path().join("db")).unwrap();
    for (n, sql) in schema::MIGRATIONS[..9].iter().enumerate() {
        c.execute_batch(sql).unwrap();
        c.execute(
            "INSERT OR REPLACE INTO meta VALUES('schema_version',?)",
            [(n + 1).to_string()],
        )
        .unwrap();
    }
    c.execute_batch("CREATE INDEX replay_events_kind ON replay_events(kind, complete, seq)")
        .unwrap();
    schema::migrate(&mut c).unwrap();
    for sql in [
        "SELECT seq,payload_json FROM replay_events WHERE kind='parent_worker_control' AND complete=0 ORDER BY seq LIMIT 128",
        "SELECT EXISTS(SELECT 1 FROM replay_events WHERE kind='parent_worker_control' AND complete=0 AND json_extract(payload_json,'$.session')='s')",
        "SELECT seq,json_extract(payload_json,'$.worker') FROM replay_events WHERE kind='thread_worker_stop' AND complete=0 ORDER BY seq",
        "SELECT EXISTS(SELECT 1 FROM replay_events WHERE kind='configuration_edit' AND complete=0)",
    ] {
        let plan: Vec<String> = c
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |r| r.get(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            plan.iter().any(|p| p.contains("USING") && p.contains("replay_events_kind"))
                && !plan.iter().any(|p| p == "SCAN replay_events"),
            "{sql}: {plan:?}"
        );
    }
}

// The migration engine lives in fridica-store-sqlite (#117) and is tested
// there; these two exercise Fridica's own configuration file as its companion.

#[test]
fn concurrent_configuration_edit_is_preserved_and_migration_can_be_reconciled() {
    use fridica::cli::migrate::{migrate, rollback, ConfigFile};
    let dir = tempfile::tempdir().unwrap();
    let (db, cfg) = (
        dir.path().join("state.sqlite3"),
        dir.path().join("config.toml"),
    );
    drop(v5(&db));
    config_file(&cfg);
    let original = std::fs::read_to_string(&cfg).unwrap();
    let edited = format!("{original}\n# edited concurrently\n");
    let error = migration::testing::migrate_with_checkpoint(&db, &cfg, 10., &ConfigFile, |phase| {
        if phase == "conversion" {
            std::fs::write(&cfg, &edited)?;
        }
        Ok(())
    })
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("configuration changed before replacement"));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), edited);
    assert!(migration::check_ready(&db).is_err());
    assert!(migrate(&db, &cfg, 20.).is_err());
    std::fs::write(&cfg, &original).unwrap();
    migrate(&db, &cfg, 30.).unwrap();
    rollback(&db, &cfg).unwrap();
}

#[test]
fn migration_respects_the_shared_configuration_writer_lock() {
    use fridica::cli::migrate::{migrate, rollback};
    use fs2::FileExt;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (db, cfg) = (
        dir.path().join("state.sqlite3"),
        dir.path().join("config.toml"),
    );
    drop(v5(&db));
    config_file(&cfg);
    let original = std::fs::read_to_string(&cfg).unwrap();
    let guard = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(cfg.with_file_name(".config.toml.edit.lock"))
        .unwrap();
    guard.try_lock_exclusive().unwrap();
    assert!(migrate(&db, &cfg, 10.)
        .unwrap_err()
        .to_string()
        .contains("another Fridica configuration edit"));
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), original);
    drop(guard);
    migrate(&db, &cfg, 20.).unwrap();
    rollback(&db, &cfg).unwrap();
}
