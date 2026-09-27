//! All schema changes live here. Existing Python schema versions are immutable.
use anyhow::{bail, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

pub const VERSION: usize = 6;
pub const MIGRATIONS: [&str; VERSION] = [
    include_str!("migrations/001.sql"),
    include_str!("migrations/002.sql"),
    include_str!("migrations/003.sql"),
    include_str!("migrations/004.sql"),
    include_str!("migrations/005.sql"),
    include_str!("migrations/006.sql"),
];

pub fn version(c: &Connection) -> Result<usize> {
    let exists: bool = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='meta' AND type='table')",
        [],
        |r| r.get(0),
    )?;
    if !exists {
        return Ok(0);
    }
    let value: Option<String> = c
        .query_row(
            "SELECT value FROM meta WHERE key='schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value.map(|s| s.parse()).transpose()?.unwrap_or(0))
}

pub fn migrate(c: &mut Connection) -> Result<()> {
    let current = version(c)?;
    if current > VERSION {
        bail!("state database schema v{current} is newer than v{VERSION}");
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current) {
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('schema_version',?)",
            [(index + 1).to_string()],
        )?;
        tx.commit()?;
    }
    Ok(())
}

/// Track *every* durable table mutation, not merely audit inserts. A write followed
/// by a compensating write must still invalidate automatic rollback.
pub fn install_mutation_guards(c: &Connection) -> Result<()> {
    let tables: Vec<String> = c
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    c.execute(
        "INSERT OR IGNORE INTO meta VALUES('durable_generation','0')",
        [],
    )?;
    for table in tables {
        let name = table.replace('"', "\"\"");
        for operation in ["INSERT", "UPDATE", "DELETE"] {
            let condition = if table == "meta" {
                if operation == "DELETE" {
                    "WHEN OLD.key!='durable_generation'"
                } else {
                    "WHEN NEW.key!='durable_generation'"
                }
            } else {
                ""
            };
            c.execute_batch(&format!(
                "CREATE TRIGGER IF NOT EXISTS \"guard_{name}_{operation}\" AFTER {operation} ON \"{name}\" {condition}
                 BEGIN UPDATE meta SET value=CAST(value AS INTEGER)+1 WHERE key='durable_generation'; END;"))?;
        }
    }
    Ok(())
}

pub fn overseer(c: &mut Connection) -> Result<()> {
    let v = version(c)?;
    if v > 0 {
        let kind: Option<String> = c
            .query_row("SELECT value FROM meta WHERE key='store_kind'", [], |r| {
                r.get(0)
            })
            .optional()?;
        if kind.as_deref() != Some("overseer") || v != 1 {
            bail!("not a supported overseer database");
        }
        return Ok(());
    }
    let tx = c.transaction()?;
    tx.execute_batch(OVERSEER)?;
    tx.commit()?;
    Ok(())
}

const OVERSEER: &str = r#"
CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
INSERT INTO meta VALUES('store_kind','overseer'),('schema_version','1');
CREATE TABLE campaigns(id TEXT PRIMARY KEY,lead TEXT NOT NULL,stopped INTEGER NOT NULL DEFAULT 0,
    revision INTEGER NOT NULL,data_json TEXT NOT NULL,updated REAL NOT NULL);
CREATE TABLE work_items(id TEXT PRIMARY KEY,campaign_id TEXT NOT NULL REFERENCES campaigns(id),
    head_sha TEXT NOT NULL,head_tree TEXT NOT NULL,revision INTEGER NOT NULL,data_json TEXT NOT NULL,updated REAL NOT NULL);
CREATE TABLE evidence(id TEXT PRIMARY KEY,item_id TEXT NOT NULL REFERENCES work_items(id),
    head_sha TEXT NOT NULL,kind TEXT NOT NULL,verified_by TEXT NOT NULL,data_json TEXT NOT NULL,verified_at REAL NOT NULL);
CREATE TABLE actions(id TEXT PRIMARY KEY,item_id TEXT NOT NULL REFERENCES work_items(id),head_sha TEXT NOT NULL,
    action TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('planned','executing','uncertain','done','failed','refused')),
    detail_json TEXT NOT NULL,created REAL NOT NULL,finished REAL,
    UNIQUE(item_id,head_sha,action));
CREATE TABLE exchanges(id TEXT PRIMARY KEY,kind TEXT NOT NULL,payload_json TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',response_json TEXT,created REAL NOT NULL,acknowledged_at REAL);
CREATE TABLE audit(id INTEGER PRIMARY KEY,time REAL NOT NULL,actor TEXT NOT NULL,action TEXT NOT NULL,
    target TEXT NOT NULL,details_json TEXT NOT NULL);
"#;
