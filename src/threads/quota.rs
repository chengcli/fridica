//! One cooldown per configured parent backend in the daemon's state directory.
//! A stable sibling lock serializes changes across actors and daemons sharing
//! a quota directory. No lock spans a parent call.
use crate::config::Config;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};
use uuid::Uuid;

fn lock(path: &Path) -> Result<fs::File> {
    let lock = crate::store::private_file(&path.with_extension("cooldown.lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}

#[derive(Serialize, Deserialize)]
struct State {
    until: f64,
    probe: Option<Uuid>,
}

pub(super) struct Permit {
    path: PathBuf,
    probe: Option<Uuid>,
}

fn read(path: &Path) -> Result<Option<State>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn write(path: &Path, state: &State) -> Result<()> {
    let mut temp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
    serde_json::to_writer(&mut temp, state)?;
    temp.as_file().sync_all()?;
    temp.persist(path)?;
    fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

fn path(config: &Config) -> PathBuf {
    if let Some(dir) = &config.state.parent_quota_dir {
        return dir.join(format!("parent-{}.cooldown", config.parent.backend));
    }
    let mut name = config.state.path.as_os_str().to_owned();
    name.push(format!(".parent-{}.cooldown", config.parent.backend));
    PathBuf::from(name)
}

impl State {
    fn wait_until(&self, now: f64) -> Option<f64> {
        // A successful probe can release the queue before its lease expires.
        (self.until > now).then_some(if self.probe.is_some() {
            self.until.min(now + 5.)
        } else {
            self.until
        })
    }
}

pub(super) fn waiting(config: &Config, now: f64) -> Result<Option<f64>> {
    Ok(read(&path(config))?.and_then(|s| s.wait_until(now)))
}

pub(super) fn enter(config: &Config, now: f64, lease: f64) -> Result<Result<Permit, f64>> {
    enter_path(path(config), now, lease)
}

fn enter_path(path: PathBuf, now: f64, lease: f64) -> Result<Result<Permit, f64>> {
    if read(&path)?.is_none() {
        return Ok(Ok(Permit { path, probe: None }));
    }
    let _guard = lock(&path)?;
    let probe = if let Some(state) = read(&path)? {
        if let Some(until) = state.wait_until(now) {
            return Ok(Err(until));
        }
        let probe = Some(Uuid::new_v4());
        write(
            &path,
            &State {
                until: now + lease,
                probe,
            },
        )?;
        probe
    } else {
        None
    };
    Ok(Ok(Permit { path, probe }))
}

impl Permit {
    pub fn finish(self, retry_at: Option<f64>, success: bool) -> Result<()> {
        if self.probe.is_none() && retry_at.is_none() {
            return Ok(());
        }
        let _guard = lock(&self.path)?;
        let prior = read(&self.path)?;
        if let Some(until) = retry_at {
            // Overlapping refusals may extend, but never shorten, a cooldown.
            let until = prior
                .filter(|s| s.probe.is_none())
                .map_or(until, |s| s.until.max(until));
            write(&self.path, &State { until, probe: None })?;
        } else if success && prior.is_some_and(|s| s.probe == self.probe) {
            fs::remove_file(self.path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_late_probe_cannot_release_a_newer_lease_or_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parent-test.cooldown");
        let enter = |now| enter_path(path.clone(), now, 30.).unwrap();
        let first = enter(0.).unwrap();
        let overlapping = enter(1.).unwrap();
        first.finish(Some(100.), false).unwrap();
        overlapping.finish(Some(80.), false).unwrap();
        assert_eq!(enter(90.).err(), Some(100.));
        let expired_probe = enter(100.).unwrap();
        let next_probe = enter(130.).unwrap();
        expired_probe.finish(None, true).unwrap();
        assert!(enter(131.).is_err());
        next_probe.finish(Some(200.), false).unwrap();
        assert_eq!(enter(150.).err(), Some(200.));
        enter(200.).unwrap().finish(None, true).unwrap();
        assert!(enter(201.).unwrap().probe.is_none());
    }
}
