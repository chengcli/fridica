//! Local transport and descriptor-based artifact reads.
use super::{
    process::{self, Launch},
    sandbox,
};
use crate::config::registry::Machine;
use anyhow::{bail, Context, Result};
use rustix::{
    fd::OwnedFd,
    fs::{open, openat, Mode, OFlags},
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

pub const ARTIFACT_LIMIT: usize = 20 * 1024 * 1024;
pub struct LocalTransport {
    pub machine: Machine,
    pub home: PathBuf,
    pub excluded_env: Vec<String>,
}
impl LocalTransport {
    pub fn launch(
        &self,
        command: Vec<String>,
        cwd: &Path,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
        extra: &BTreeMap<String, String>,
        confine: Option<&[String]>,
        create: bool,
    ) -> Result<Launch> {
        if self.machine.transport != "local" {
            bail!("local transport requires a local machine");
        }
        super::shell::validate(&command)?;
        let cwd = expand_home(cwd, &self.home);
        if !cwd.is_absolute() {
            bail!("local workspace must be absolute");
        }
        if create {
            fs::create_dir_all(&cwd)?;
        }
        let mut argv = if let Some(roots) = confine {
            sandbox::prepare_local(&self.home)?;
            sandbox::confinement(
                roots,
                Some(self.home.to_str().context("home is not UTF-8")?),
            )?
        } else {
            vec![]
        };
        argv.extend(command);
        let mut environment = self.machine.resources.environment();
        environment.extend(extra.clone());
        Ok(Launch {
            argv,
            cwd: Some(cwd),
            env: process::scrubbed_environment(inherited, &self.excluded_env, &environment),
        })
    }
    pub async fn read_file(
        &self,
        path: PathBuf,
        roots: Vec<PathBuf>,
        limit: usize,
    ) -> Result<Vec<u8>> {
        let home = self.home.clone();
        tokio::task::spawn_blocking(move || {
            read_file(
                &expand_home(&path, &home),
                &roots
                    .iter()
                    .map(|r| expand_home(r, &home))
                    .collect::<Vec<_>>(),
                limit,
            )
        })
        .await?
    }
}
fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(tail) => home.join(tail),
        Err(_) => path.to_path_buf(),
    }
}
/// Open every canonical path component without following symlinks. This closes
/// the canonicalize/open race, including replacement of a root's ancestors.
fn open_directory(path: &Path) -> Result<OwnedFd> {
    let mut directory = open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = openat(
                    directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?
            }
            _ => bail!("invalid canonical artifact root"),
        }
    }
    Ok(directory)
}
pub fn read_file(path: &Path, roots: &[PathBuf], limit: usize) -> Result<Vec<u8>> {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        bail!("artifact path must be absolute without parent traversal");
    }
    // Allow stable symlinks whose resolved target stays in a configured root.
    // A symlink appearing after this resolution is rejected by openat NOFOLLOW.
    let resolved = fs::canonicalize(path).context("artifact does not exist")?;
    let mut selected = None;
    for root in roots {
        let Ok(root) = fs::canonicalize(root) else {
            continue;
        };
        if let Ok(relative) = resolved.strip_prefix(&root) {
            selected = Some((root.clone(), relative.to_owned()));
            break;
        }
    }
    let (root, relative) = selected.context("artifact is outside the workspace")?;
    let mut directory = open_directory(&root)?;
    let components: Vec<_> = relative.components().collect();
    let Some((Component::Normal(last), parents)) = components.split_last() else {
        bail!("artifact is not a regular file");
    };
    for component in parents {
        let Component::Normal(name) = component else {
            bail!("invalid artifact path");
        };
        directory = openat(
            directory,
            *name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
    }
    // NONBLOCK prevents a raced-in FIFO from hanging the blocking reader.
    let fd = openat(
        directory,
        *last,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?;
    let file = fs::File::from(fd);
    if !file.metadata()?.is_file() {
        bail!("artifact is not a regular file");
    }
    let mut data = vec![];
    file.take(
        u64::try_from(limit)?
            .checked_add(1)
            .context("artifact limit too large")?,
    )
    .read_to_end(&mut data)?;
    if data.len() > limit {
        bail!("artifact exceeded the size limit");
    }
    Ok(data)
}
