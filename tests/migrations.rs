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
    let plan = migration::dry_run(&db, &cfg).unwrap();
    assert_eq!(plan.from, 5);
    assert_eq!(plan.automatic_pauses, 1);
    assert_eq!(std::fs::read(&cfg).unwrap(), original);
    migration::migrate(&db, &cfg, 10.).unwrap();
    migration::migrate(&db, &cfg, 11.).unwrap();
    let c = Connection::open(&db).unwrap();
    assert_eq!(schema::version(&c).unwrap(), 6);
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
    migration::rollback(&db, &cfg).unwrap();
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
    migration::migrate(&db, &cfg, 1.).unwrap();
    let c = Connection::open(&db).unwrap();
    c.execute(
        "INSERT INTO health_events(kind,details_json,created) VALUES('disconnect','{}',2)",
        [],
    )
    .unwrap();
    c.execute("DELETE FROM health_events", []).unwrap();
    assert!(migration::rollback(&db, &cfg)
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
    migration::migrate(&db, &cfg, 1.).unwrap();
    let path = dir.path().join("state.sqlite3.migration.json");
    let mut journal: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    journal["phase"] = "prepared".into();
    journal["generation"] = serde_json::Value::Null;
    std::fs::write(&path, serde_json::to_vec(&journal).unwrap()).unwrap();
    std::fs::write(&cfg, original).unwrap();
    assert!(migration::check_ready(&db).is_err());
    migration::migrate(&db, &cfg, 2.).unwrap();
    migration::check_ready(&db).unwrap();
    migration::rollback(&db, &cfg).unwrap();
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
    assert!(migration::migrate(&db, &cfg, 1.)
        .unwrap_err()
        .to_string()
        .contains("fetch_repos"));
    assert_eq!(schema::version(&Connection::open(&db).unwrap()).unwrap(), 5);
    assert!(!dir.path().join("state.sqlite3.pre-v6").exists());
    assert!(!dir.path().join("config.toml.pre-v6").exists());
    assert!(!dir.path().join("state.sqlite3.migration.json").exists());
    std::fs::write(&cfg, original.replace("state.sqlite3", "other.sqlite3")).unwrap();
    assert!(migration::dry_run(&db, &cfg)
        .unwrap_err()
        .to_string()
        .contains("differs"));
}
