//! Offline migration with a recoverable file journal. Never replace a live SQLite
//! file; rollback uses SQLite's backup API under the same lock as the daemon.
use super::{lock, private_file, schema};
use crate::config;
use anyhow::{bail, Context, Result};
use rusqlite::{backup::Backup, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub from: usize,
    pub to: usize,
    pub config_changes: bool,
    pub automatic_pauses: usize,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    database: PathBuf,
    config: PathBuf,
    from: usize,
    config_before: String,
    config_after: String,
    backup_hash: String,
    phase: String,
    generation: Option<i64>,
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn check_ready(database: &Path) -> Result<()> {
    let path = sibling(database, ".migration.json");
    if path.exists() {
        let journal: Journal = serde_json::from_slice(&fs::read(path)?)?;
        if journal.phase != "complete" {
            bail!("unfinished migration: run migrate or rollback before starting");
        }
    }
    Ok(())
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    // Unique temporary files prevent a crashed writer's leftovers from being trusted.
    let tmp = sibling(path, &format!(".{}.tmp", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn read_connection(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

fn automatic(reason: &str) -> bool {
    let Some((n, suffix)) = reason.split_once(' ') else {
        return false;
    };
    n.parse::<u32>().is_ok_and(|v| v > 0)
        && matches!(
            suffix,
            "consecutive replies needed more information; possible conversation loop."
                | "turns without progress; review before continuing."
        )
}

pub fn dry_run(database: &Path, configuration: &Path) -> Result<Plan> {
    let source = fs::read_to_string(configuration)?;
    let edited = config::migrate_text(&source)?;
    let context = config::LoadContext::current()?;
    let resolved = config::loader::parse(&source, configuration, &context)?;
    let target = config::loader::resolve_path(database, &std::env::current_dir()?, &context.home)?;
    if resolved.state.path != target {
        bail!("migration database differs from configuration state.path");
    }
    let c = read_connection(database)?;
    let from = schema::version(&c)?;
    if from > schema::VERSION {
        bail!("database is newer than this runtime");
    }
    let count = if from == 0 {
        0
    } else {
        c.prepare("SELECT pause_reason FROM threads WHERE control='paused'")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .filter(|s| automatic(s))
            .count()
    };
    Ok(Plan {
        from,
        to: schema::VERSION,
        config_changes: source != edited,
        automatic_pauses: count,
    })
}

pub fn migrate(database: &Path, configuration: &Path, now: f64) -> Result<Plan> {
    let _guard = lock(database)?;
    let database = fs::canonicalize(database)?;
    let configuration = fs::canonicalize(configuration)?;
    let journal_path = sibling(&database, ".migration.json");
    let backup_path = sibling(&database, ".pre-v6");
    let config_backup = sibling(&configuration, ".pre-v6");
    let plan = dry_run(&database, &configuration)?;
    let mut journal: Journal = if journal_path.exists() {
        let prior: Journal = serde_json::from_slice(&fs::read(&journal_path)?)?;
        if prior.database != database || prior.config != configuration {
            bail!("migration journal path mismatch");
        }
        if prior.phase == "complete" && plan.from == schema::VERSION {
            return Ok(plan);
        }
        if prior.phase == "rolled_back" {
            bail!("archive the completed rollback journal before a new migration");
        }
        prior
    } else {
        if plan.from == schema::VERSION && !plan.config_changes {
            return Ok(plan);
        }
        if backup_path.exists() || config_backup.exists() {
            bail!("backup already exists without a migration journal; preserve it before retrying");
        }
        let source = fs::read_to_string(&configuration)?;
        let after = config::migrate_text(&source)?;
        // Write the recovery intent before creating either backup. A crash at any
        // following instruction is resumed using these immutable fingerprints.
        let journal = Journal {
            database: database.clone(),
            config: configuration.clone(),
            from: plan.from,
            config_before: digest(source.as_bytes()),
            config_after: after,
            backup_hash: String::new(),
            phase: "preparing".into(),
            generation: None,
        };
        atomic_write(&journal_path, &serde_json::to_vec_pretty(&journal)?)?;
        journal
    };
    let current_config = fs::read(&configuration)?;
    if digest(&current_config) != journal.config_before
        && current_config != journal.config_after.as_bytes()
    {
        bail!("configuration changed since migration began; refusing to overwrite it");
    }
    let mut c = Connection::open(&database)?;
    c.execute_batch("PRAGMA foreign_keys=ON;")?;
    if journal.phase == "preparing" {
        if schema::version(&c)? != journal.from {
            bail!("database changed before migration backup completed");
        }
        // No database mutations have happened yet; safely rebuild a partial backup.
        private_file(&backup_path)?;
        let mut backup = Connection::open(&backup_path)?;
        Backup::new(&c, &mut backup)?.run_to_completion(128, Duration::from_millis(1), None)?;
        drop(backup);
        fs::File::open(&backup_path)?.sync_all()?;
        if !config_backup.exists() {
            atomic_write(&config_backup, &current_config)?;
        }
        if digest(&fs::read(&config_backup)?) != journal.config_before {
            bail!("configuration backup fingerprint mismatch");
        }
        journal.backup_hash = digest(&fs::read(&backup_path)?);
        journal.phase = "prepared".into();
        atomic_write(&journal_path, &serde_json::to_vec_pretty(&journal)?)?;
    }
    if digest(&fs::read(&backup_path)?) != journal.backup_hash {
        bail!("database backup fingerprint mismatch");
    }
    schema::migrate(&mut c)?;
    // This transaction is idempotent across interruptions. System resume events
    // and their effects commit together; manual and unrecognized pauses remain.
    let tx = c.transaction()?;
    let converted: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='v6_converted')",
        [],
        |r| r.get(0),
    )?;
    if !converted {
        let threads: Vec<(String, String, String)> = tx
            .prepare("SELECT id,control,pause_reason FROM threads")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, control, reason) in threads {
            let known = control == "paused" && automatic(&reason);
            let control_json = if known || control == "active" {
                json!({"kind":"active"})
            } else if control == "paused" {
                json!({"kind":"paused","by":{"kind":"owner"},"reason":reason,"since":now})
            } else {
                json!({"kind":control})
            };
            tx.execute(
                "UPDATE threads SET control_json=? WHERE id=?",
                rusqlite::params![control_json.to_string(), id],
            )?;
            if known {
                let payload = json!({"action":"resume","actor":"system","reason":reason});
                tx.execute("INSERT INTO thread_inbox(session_id,kind,payload_json,state,created,dedup_key) VALUES(?,'control',?,'done',?,?)",
                    rusqlite::params![id,payload.to_string(),now,format!("migration-resume:{id}")])?;
                tx.execute("UPDATE threads SET control='active',pause_reason='',wait_streak=0,no_progress=0,version=version+1,updated=? WHERE id=?", rusqlite::params![now,id])?;
                tx.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,'system','migration.resume',?,?)", rusqlite::params![now,id,payload.to_string()])?;
            }
        }
        tx.execute("UPDATE messages SET mentions_owner=1 WHERE instr(text,'<@'||(SELECT value FROM meta WHERE key='owner')||'>')>0", [])?;
        let watermarks: Vec<(String, String)> = tx
            .prepare("SELECT key,value FROM meta WHERE key LIKE 'catchup:%'")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (key, value) in watermarks {
            let parts: Vec<_> = key.split(':').collect();
            if parts.len() == 3 {
                let timestamp: f64 = value.parse().context("invalid catch-up watermark")?;
                if !timestamp.is_finite() {
                    bail!("invalid catch-up watermark");
                }
                tx.execute("INSERT OR IGNORE INTO channel_watermarks(workspace,channel,last_complete_pass) VALUES(?,?,?)",rusqlite::params![parts[1],parts[2],timestamp])?;
            }
        }
        tx.execute("INSERT INTO meta VALUES('v6_converted','1')", [])?;
    }
    schema::install_mutation_guards(&tx)?;
    tx.commit()?;
    atomic_write(&configuration, journal.config_after.as_bytes())?;
    journal.generation = Some(c.query_row(
        "SELECT CAST(value AS INTEGER) FROM meta WHERE key='durable_generation'",
        [],
        |r| r.get(0),
    )?);
    journal.phase = "complete".into();
    atomic_write(&journal_path, &serde_json::to_vec_pretty(&journal)?)?;
    Ok(plan)
}

pub fn rollback(database: &Path, configuration: &Path) -> Result<()> {
    let _guard = lock(database)?;
    let database = fs::canonicalize(database)?;
    let configuration = fs::canonicalize(configuration)?;
    let journal_path = sibling(&database, ".migration.json");
    let mut journal: Journal = serde_json::from_slice(&fs::read(&journal_path)?)?;
    if journal.database != database || journal.config != configuration {
        bail!("migration journal path mismatch");
    }
    if !matches!(journal.phase.as_str(), "complete" | "rolling_back") {
        bail!("migration is not complete");
    }
    let mut c = Connection::open(&database)?;
    if journal.phase == "complete" {
        let generation: i64 = c.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='durable_generation'",
            [],
            |r| r.get(0),
        )?;
        if Some(generation) != journal.generation {
            bail!("durable mutations occurred after migration; explicit backup restoration is required");
        }
        if fs::read(&configuration)? != journal.config_after.as_bytes() {
            bail!("configuration changed after migration");
        }
    }
    let backup_path = sibling(&database, ".pre-v6");
    let config_backup = fs::read(sibling(&configuration, ".pre-v6"))?;
    if digest(&fs::read(&backup_path)?) != journal.backup_hash
        || digest(&config_backup) != journal.config_before
    {
        bail!("backup fingerprint mismatch");
    }
    journal.phase = "rolling_back".into();
    atomic_write(&journal_path, &serde_json::to_vec_pretty(&journal)?)?;
    let source = read_connection(&backup_path)?;
    Backup::new(&source, &mut c)?
        .run_to_completion(128, Duration::from_millis(1), None)
        .context("restore database backup")?;
    atomic_write(&configuration, &config_backup)?;
    journal.phase = "rolled_back".into();
    atomic_write(&journal_path, &serde_json::to_vec_pretty(&journal)?)?;
    Ok(())
}
