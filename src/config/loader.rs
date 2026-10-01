//! Configuration loading is read-only. Migration writes are a separate operation.
use super::{
    migrate_text,
    registry::{self, Machine, Policy, Registry, Resources, Slurm, Workspace, BACKENDS},
    schema::*,
    Attention,
};
use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
};
use toml_edit::{DocumentMut, Item, TableLike};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;
use users::os::unix::UserExt;

/// Inject host context for deterministic configuration replay; do not alter HOME.
#[derive(Clone, Debug)]
pub struct LoadContext {
    pub home: PathBuf,
    pub runtime_dir: Option<PathBuf>,
    pub uid: u32,
    /// Executable/package roots that writable local workspaces must not contain.
    pub protected: Vec<PathBuf>,
}
impl LoadContext {
    pub fn current() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .or_else(|| {
                users::get_user_by_uid(users::get_current_uid()).map(|u| u.home_dir().to_path_buf())
            })
            .context("home directory is not configured")?;
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir());
        Ok(Self {
            home,
            runtime_dir,
            uid: users::get_current_uid(),
            protected: vec![std::env::current_exe()?],
        })
    }
}
pub fn load(path: &Path, context: &LoadContext) -> Result<Config> {
    let path = resolve_path(path, &std::env::current_dir()?, &context.home)?;
    let bytes = std::fs::read(&path).with_context(|| {
        format!(
            "{} cannot be read; run fridica init for a new installation",
            path.display()
        )
    })?;
    let source = std::str::from_utf8(&bytes).context("configuration must be UTF-8")?;
    parse(source, &path, context)
        .with_context(|| format!("invalid configuration {}", path.display()))
}
pub fn parse(source: &str, path: &Path, context: &LoadContext) -> Result<Config> {
    let path = resolve_path(path, &std::env::current_dir()?, &context.home)?;
    let base = path
        .parent()
        .context("configuration needs a parent directory")?;
    let doc = migrate_text(source)?.parse::<DocumentMut>()?;
    let root = doc.as_table();
    keys(
        root,
        &[
            "owner",
            "slack",
            "parent",
            "limits",
            "policy",
            "machines",
            "state",
            "github",
            "attention",
            "isolation",
            "placement",
            "egress",
        ],
        "top level",
    )?;
    for key in ["owner", "slack", "machines"] {
        if root.get(key).is_none() {
            bail!("missing [{key}] section");
        }
    }
    let mut owner: Owner = decode(root.get("owner"))?;
    if !slack_id(&owner.slack_user, "UW") {
        bail!("owner.slack_user must be a Slack member ID");
    }
    if owner.profile.chars().count() > 4000 {
        bail!("owner.profile must have at most 4000 characters");
    }
    owner.contract = match owner.contract {
        Some(p) => Some(file(&p, base, context)?),
        None if base.join("contract.md").is_file() => Some(resolve_path(
            &base.join("contract.md"),
            base,
            &context.home,
        )?),
        None => None,
    };
    let slack: Slack = decode(root.get("slack"))?;
    if !slack_id(&slack.workspace, "T") {
        bail!("slack.workspace must be a Slack team ID");
    }
    if slack.channels.is_empty() || !channel_ids(&slack.channels) {
        bail!("slack.channels must list unique channel IDs");
    }
    if let Some(ids) = &slack.delegate_channels {
        if !channel_ids(ids) || ids.iter().any(|id| !slack.channels.contains(id)) {
            bail!("slack.delegate_channels must be a unique subset of slack.channels");
        }
    }
    if !env_name(&slack.app_token_env) || !env_name(&slack.user_token_env) {
        bail!("Slack token settings must name environment variables");
    }
    number(slack.cooldown, 0., "slack.cooldown")?;
    let mut parent: Parent = decode(root.get("parent"))?;
    if !BACKENDS.contains(&parent.backend.as_str()) {
        bail!("parent.backend must be claude or codex");
    }
    if parent.model.chars().count() > 200 || parent.triage_model.chars().count() > 200 {
        bail!("parent models must have at most 200 characters");
    }
    if !["", "low", "medium", "high", "xhigh", "max"].contains(&parent.reasoning_effort.as_str()) {
        bail!("invalid parent.reasoning_effort");
    }
    number(parent.timeout, 1., "parent.timeout")?;
    if parent.context_chars < 2000 {
        bail!("parent.context_chars must be at least 2000");
    }
    parent.repos = parent.repos.map(|p| file(&p, base, context)).transpose()?;
    let limits: Limits = decode(root.get("limits"))?;
    let placement: Placement = decode(root.get("placement"))?;
    number(placement.probe_ttl, 0., "placement.probe_ttl")?;
    number(placement.probe_timeout, 1., "placement.probe_timeout")?;
    if placement.probe_timeout > 120. {
        bail!("placement.probe_timeout must be at most 120 seconds");
    }
    for (value, name) in [
        (placement.max_load, "placement.max_load"),
        (
            placement.max_gpu_utilization,
            "placement.max_gpu_utilization",
        ),
        (placement.max_gpu_memory, "placement.max_gpu_memory"),
    ] {
        if !value.is_finite() || value <= 0. {
            bail!("{name} must be a positive number");
        }
    }
    if [
        limits.max_delegations_per_turn,
        limits.max_workers_per_thread,
        limits.max_jobs,
        limits.parent_concurrency,
    ]
    .contains(&0)
    {
        bail!("limits counts must be positive integers");
    }
    for (name, n) in [
        ("job_timeout", limits.job_timeout),
        ("worker_idle", limits.worker_idle),
        ("session_timeout", limits.session_timeout),
    ] {
        number(n, 1., &format!("limits.{name}"))?;
    }
    if !(500..=12000).contains(&limits.reply_chars) {
        bail!("limits.reply_chars must be between 500 and 12000");
    }
    let policy: Policy = decode(root.get("policy"))?;
    policy.validate()?;
    let attention: Attention = decode(root.get("attention"))?;
    attention.validate()?;
    let mut egress: Egress = decode(root.get("egress"))?;
    if let Some(path) = egress.deny_list.take() {
        let path = file(&path, base, context)?;
        super::egress::deny_list(&path).context("invalid egress.deny_list")?;
        egress.deny_list = Some(path);
    }
    let github: GitHub = decode(root.get("github"))?;
    if !env_name(&github.token_env) {
        bail!("github.token_env must name an environment variable");
    }
    number(github.cache_seconds, 0., "github.cache_seconds")?;
    let state_data = table(root.get("state"))?;
    if let Some(t) = state_data {
        keys(t, &["path", "control_socket"], "state")?;
    }
    let state_path = string(state_data.and_then(|t| t.get("path")))?
        .unwrap_or_else(|| "~/.local/state/fridica/state.sqlite3".into());
    let state_path = resolve_path(Path::new(&state_path), base, &context.home)?;
    let control_socket = if let Some(text) =
        string(state_data.and_then(|t| t.get("control_socket")))?
    {
        let p = resolve_path(Path::new(&text), base, &context.home)?;
        if p.as_os_str().len() > 100 {
            bail!("state.control_socket path is too long for a Unix socket");
        }
        p
    } else {
        let p = state_path.with_file_name("control.sock");
        if p.as_os_str().len() <= 100 {
            p
        } else {
            let directory = context
                .runtime_dir
                .as_ref()
                .filter(|p| p.is_dir())
                .map(|p| p.join("fridica"))
                .unwrap_or_else(|| PathBuf::from(format!("/tmp/fridica-{}", context.uid)));
            let hash = format!(
                "{:x}",
                Sha256::digest(state_path.to_string_lossy().as_bytes())
            );
            let fallback = directory.join(format!("control-{}.sock", &hash[..12]));
            // A long XDG_RUNTIME_DIR cannot make an unusable socket pass validation.
            if fallback.as_os_str().len() > 100 {
                bail!("runtime control socket path is too long; configure a shorter state.control_socket");
            }
            fallback
        }
    };
    let machine_table = table(root.get("machines"))?.context("machines must be a table")?;
    let mut machines = Vec::new();
    for (name, item) in machine_table.iter() {
        machines.push(machine(
            name,
            item,
            &policy,
            &parent.backend,
            base,
            context,
        )?);
    }
    if parent.default_machine.is_empty() {
        parent.default_machine = machines.first().map(|m| m.name.clone()).unwrap_or_default();
    }
    let registry = Registry {
        machines,
        default: parent.default_machine.clone(),
    };
    registry.validate()?;
    // Workers see the target's normal files so git push/commit and gh work; only
    // Fridica's own daemon files are hidden, automatically.
    if let Some(table) = root.get("isolation").and_then(Item::as_table_like) {
        let remote = table.get("remote").and_then(Item::as_table_like);
        if table.contains_key("private_files")
            || remote.is_some_and(|r| {
                r.iter().any(|(_, t)| {
                    t.as_table_like()
                        .is_some_and(|t| t.contains_key("private_files"))
                })
            })
        {
            bail!("isolation private_files was removed: workers now see the target's normal files; delete the setting");
        }
    }
    let mut isolation: super::isolation::Settings = decode(root.get("isolation"))?;
    for path in isolation.settings_files.iter_mut() {
        *path = resolve_path(path, base, &context.home)?;
    }
    isolation.validate(&registry)?;
    let config = Config {
        owner,
        slack,
        parent,
        limits,
        policy,
        machines: registry,
        state: State {
            path: state_path,
            control_socket,
        },
        github,
        attention,
        isolation,
        placement,
        egress,
        path,
        fingerprint: format!("{:x}", Sha256::digest(source.as_bytes())),
    };
    protect(&config, context)?;
    Ok(config)
}

fn machine(
    name: &str,
    item: &Item,
    global: &Policy,
    backend: &str,
    base: &Path,
    context: &LoadContext,
) -> Result<Machine> {
    if !registry::name(name) {
        bail!("machine name {name:?} must be lowercase letters, digits, - or _");
    }
    let t = table(Some(item))?.context("machine must be a table")?;
    keys(
        t,
        &[
            "transport",
            "host",
            "tags",
            "backends",
            "default_backend",
            "max_workers",
            "max_jobs",
            "policy",
            "resources",
            "slurm",
            "description",
            "workspaces",
        ],
        "machine",
    )?;
    let transport = string(t.get("transport"))?
        .unwrap_or_else(|| if name == "local" { "local" } else { "ssh" }.into());
    if !["local", "ssh", "slurm"].contains(&transport.as_str()) {
        bail!("invalid machine transport");
    }
    let host = string(t.get("host"))?.unwrap_or_default();
    if transport == "local" && !host.is_empty() {
        bail!("local machine takes no host");
    }
    if transport != "local" && !registry::ssh_host(&host) {
        bail!("host must be an SSH alias or user@host");
    }
    let tags: Vec<String> = field(t, "tags", vec![])?;
    if tags.iter().any(|s| !registry::name(s)) {
        bail!("tags must list lowercase names");
    }
    let backends: Vec<String> = field(t, "backends", vec![backend.into()])?;
    if backends.is_empty()
        || backends.iter().any(|b| !BACKENDS.contains(&b.as_str()))
        || backends.iter().collect::<HashSet<_>>().len() != backends.len()
    {
        bail!("backends must list claude and/or codex");
    }
    let default_backend = string(t.get("default_backend"))?.unwrap_or_else(|| backends[0].clone());
    if !backends.contains(&default_backend) {
        bail!("default_backend must be one of the machine backends");
    }
    let max_workers: usize = field(t, "max_workers", 4)?;
    let max_jobs: usize = field(t, "max_jobs", 2)?;
    if max_workers == 0 || max_jobs == 0 || max_jobs > max_workers {
        bail!("max_jobs and max_workers must be positive; max_jobs cannot exceed max_workers");
    }
    let mut policy = global.override_with(object(t.get("policy"))?)?;
    let resources: Resources = decode(t.get("resources"))?;
    resources.validate()?;
    let has_gpus = resources.gpus.as_ref().is_some_and(|v| !v.is_empty());
    if policy.gpu_confine == Some(true) && !has_gpus {
        bail!("policy.gpu_confine needs resources.gpus");
    }
    let slurm = if transport == "slurm" {
        Some(decode::<Slurm>(t.get("slurm"))?)
    } else {
        if t.get("slurm").is_some() {
            bail!("slurm options need transport = slurm");
        }
        None
    };
    let workspaces_data =
        table(t.get("workspaces"))?.context("machine needs at least one entry under workspaces")?;
    let mut workspaces = vec![];
    for (name, item) in workspaces_data.iter() {
        if !registry::name(name) {
            bail!("workspace names must be lowercase letters, digits, - or _");
        }
        let (path, mut grant, subfolders) = if let Some(w) = item.as_table_like() {
            keys(w, &["path", "policy", "subfolders"], "workspace")?;
            (
                string(w.get("path"))?.context("workspace needs a path")?,
                policy.override_with(object(w.get("policy"))?)?,
                w.get("subfolders")
                    .map(|v| value(v).and_then(|v| Ok(serde_json::from_value::<bool>(v)?)))
                    .transpose()?,
            )
        } else {
            (
                string(Some(item))?.context("workspace needs a path")?,
                policy.clone(),
                None,
            )
        };
        let subfolders = subfolders.unwrap_or(grant.mode != "read-only");
        if subfolders && grant.mode == "read-only" {
            bail!("subfolders need a writable workspace");
        }
        if grant.gpu_confine == Some(true) && !has_gpus {
            bail!("workspace gpu_confine needs resources.gpus");
        }
        if grant.gpu_confine == Some(true) && grant.mode == "read-only" {
            bail!("gpu_confine cannot enforce read-only");
        }
        grant.gpu_confine = Some(
            grant
                .gpu_confine
                .unwrap_or(has_gpus && grant.mode == "write"),
        );
        if !grant.fetch_repos.is_empty()
            && (grant.mode != "write"
                || !grant.network.is_empty()
                || grant.approvals == "auto"
                || !grant.auto_approve.is_empty()
                || grant.gpu_confine == Some(true))
        {
            bail!("fetch_repos needs write mode, no worker network, non-auto approvals, no auto-approved commands, and no GPU confinement");
        }
        let path = if transport == "local" {
            let p = resolve_path(Path::new(&path), base, &context.home)?;
            if !p.is_dir() {
                bail!("workspace is not an existing directory");
            }
            if p == Path::new("/") || p == resolve_path(&context.home, base, &context.home)? {
                bail!("home directory or / is too broad for a workspace");
            }
            p
        } else {
            remote_path(&path)?
        };
        workspaces.push(Workspace {
            name: name.into(),
            path,
            policy: grant,
            subfolders,
        });
    }
    if workspaces.is_empty() {
        bail!("machine needs at least one workspace");
    }
    policy.gpu_confine = Some(policy.gpu_confine.unwrap_or(false));
    let description = string(t.get("description"))?.unwrap_or_default();
    if description.chars().count() > 1000 {
        bail!("description must have at most 1000 characters");
    }
    Ok(Machine {
        name: name.into(),
        transport,
        workspaces,
        backends,
        default_backend,
        policy,
        host,
        tags,
        resources,
        max_workers,
        max_jobs,
        slurm,
        description,
    })
}
fn remote_path(s: &str) -> Result<PathBuf> {
    if !(s.starts_with('/') || s.starts_with("~/")) {
        bail!("remote paths must be absolute or start with ~/");
    }
    if s.split('/').any(|s| s == "..") {
        bail!("remote paths may not contain ..");
    }
    let prefix = if s.starts_with("//") && !s.starts_with("///") {
        "//"
    } else if s.starts_with('/') {
        "/"
    } else {
        ""
    };
    let text = format!(
        "{prefix}{}",
        s.split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect::<Vec<_>>()
            .join("/")
    );
    if ["/", "//", "~"].contains(&text.as_str()) {
        bail!("remote home directory or / is too broad for a workspace");
    }
    Ok(text.into())
}
fn protect(config: &Config, context: &LoadContext) -> Result<()> {
    let mut protected = context.protected.clone();
    protected.push(config.path.clone());
    protected.extend(config.owner.contract.iter().cloned());
    protected.extend(config.parent.repos.iter().cloned());
    for machine in &config.machines.machines {
        if machine.remote() {
            continue;
        }
        for workspace in &machine.workspaces {
            if within(&config.state.path, &workspace.path, context)? {
                bail!(
                    "state.path must be outside workspace {}:{}",
                    machine.name,
                    workspace.name
                );
            }
            // v0.4 control capabilities are secrets too, even when explicitly placed.
            if within(&config.state.control_socket, &workspace.path, context)? {
                bail!(
                    "state.control_socket must be outside workspace {}:{}",
                    machine.name,
                    workspace.name
                );
            }
            if workspace.writable() {
                for path in &protected {
                    if within(path, &workspace.path, context)? {
                        bail!(
                            "{} is inside writable workspace {}:{}",
                            path.display(),
                            machine.name,
                            workspace.name
                        );
                    }
                }
            }
        }
    }
    Ok(())
}
fn within(path: &Path, root: &Path, context: &LoadContext) -> Result<bool> {
    Ok(casefold_path(&resolve_path(
        path,
        &std::env::current_dir()?,
        &context.home,
    )?)
    .starts_with(casefold_path(root)))
}
/// Conservative path comparison, matching the Python NFD/casefold protection.
/// Rust's lowercase tables cover newer simple mappings (e.g. Mtavruli); the
/// full-fold iterator also handles expansions and Cherokee's uppercase fold.
pub fn casefold_path(path: &Path) -> PathBuf {
    PathBuf::from(
        path.to_string_lossy()
            .nfd()
            .flat_map(char::to_lowercase)
            .case_fold()
            .collect::<String>(),
    )
}
fn file(path: &Path, base: &Path, context: &LoadContext) -> Result<PathBuf> {
    let p = resolve_path(path, base, &context.home)?;
    if !p.is_file() {
        bail!("{} is not a file", p.display());
    }
    Ok(p)
}
/// Canonicalize existing components before applying '..'; also handles new state files.
pub fn resolve_path(path: &Path, base: &Path, home: &Path) -> Result<PathBuf> {
    let text = path.to_str().context("configuration paths must be UTF-8")?;
    if text.is_empty() || text.contains('\0') {
        bail!("path must be nonempty and contain no NUL");
    }
    let path = if text == "~" {
        home.to_path_buf()
    } else if let Some(tail) = text.strip_prefix("~/") {
        home.join(tail)
    } else if let Some(named) = text.strip_prefix('~') {
        let (name, tail) = named.split_once('/').unwrap_or((named, ""));
        let user =
            users::get_user_by_name(name).context("cannot expand unknown user's home directory")?;
        user.home_dir().join(tail)
    } else {
        path.to_path_buf()
    };
    let path = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    let mut resolved = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            p => {
                resolved.push(p.as_os_str());
                match std::fs::symlink_metadata(&resolved) {
                    Ok(m) if m.file_type().is_symlink() => {
                        resolved = std::fs::canonicalize(&resolved)
                            .context("cannot resolve path symlink")?;
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    Ok(resolved)
}
pub(crate) fn slack_id(s: &str, prefixes: &str) -> bool {
    s.len() > 1
        && s.is_ascii()
        && prefixes.as_bytes().contains(&s.as_bytes()[0])
        && s[1..]
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}
fn channel_ids(ids: &[String]) -> bool {
    ids.iter().all(|s| slack_id(s, "CG")) && ids.iter().collect::<HashSet<_>>().len() == ids.len()
}
pub(crate) fn env_name(s: &str) -> bool {
    !s.is_empty()
        && (s.as_bytes()[0].is_ascii_alphabetic() || s.starts_with('_'))
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn number(n: f64, min: f64, label: &str) -> Result<()> {
    if !n.is_finite() || n < min {
        bail!("{label} must be a finite number of at least {min}");
    }
    Ok(())
}
fn keys(t: &dyn TableLike, allowed: &[&str], label: &str) -> Result<()> {
    for (k, _) in t.iter() {
        if !allowed.contains(&k) {
            bail!("unknown keys in {label}: {k}");
        }
    }
    Ok(())
}
fn table(item: Option<&Item>) -> Result<Option<&dyn TableLike>> {
    item.map(|i| i.as_table_like().context("expected a table"))
        .transpose()
}
fn string(item: Option<&Item>) -> Result<Option<String>> {
    item.map(|i| i.as_str().map(str::to_owned).context("expected a string"))
        .transpose()
}
fn decode<T: DeserializeOwned>(item: Option<&Item>) -> Result<T> {
    Ok(serde_json::from_value(object(item)?)?)
}
fn object(item: Option<&Item>) -> Result<Value> {
    match table(item)? {
        Some(t) => table_value(t),
        None => Ok(Value::Object(Map::new())),
    }
}
fn field<T: DeserializeOwned>(t: &dyn TableLike, key: &str, default: T) -> Result<T> {
    match t.get(key) {
        Some(i) => Ok(serde_json::from_value(value(i)?)?),
        None => Ok(default),
    }
}
fn table_value(t: &dyn TableLike) -> Result<Value> {
    Ok(Value::Object(
        t.iter()
            .map(|(k, v)| Ok((k.to_owned(), value(v)?)))
            .collect::<Result<_>>()?,
    ))
}
fn value(item: &Item) -> Result<Value> {
    if let Some(t) = item.as_table_like() {
        return table_value(t);
    }
    match item.as_value().context("unsupported configuration value")? {
        toml_edit::Value::String(v) => Ok(Value::String(v.value().clone())),
        toml_edit::Value::Integer(v) => Ok((*v.value()).into()),
        toml_edit::Value::Float(v) => Ok(serde_json::Number::from_f64(*v.value())
            .context("configuration numbers must be finite")?
            .into()),
        toml_edit::Value::Boolean(v) => Ok((*v.value()).into()),
        toml_edit::Value::Array(v) => Ok(Value::Array(
            v.iter()
                .map(|v| value(&Item::Value(v.clone())))
                .collect::<Result<_>>()?,
        )),
        _ => bail!("unsupported configuration value"),
    }
}
