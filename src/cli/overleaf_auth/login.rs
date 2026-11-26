//! Standalone owner setup. Only config, bundled resources and a child Node process;
//! no listener, worker inventory, model client or credential read during preparation.
use crate::config::{
    editor, loader,
    overleaf::{Scope, Settings},
    setup, LoadContext,
};
use crate::research::overleaf::{node, resources};
use anyhow::{ensure, Context, Result};
use clap::Args;
use sha2::{Digest, Sha256};
use std::{
    fs,
    os::unix::fs::{symlink, DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};
use toml_edit::{value, DocumentMut, Item, Table};

const HELPER: &str = include_str!("../../../integrations/overleaf/owner-login.mjs");

#[derive(Args)]
pub struct Options {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Expected existing Overleaf account; saved in owner configuration.
    #[arg(long)]
    expected_email: Option<String>,
    /// Optional legacy restriction to one Overleaf project ID.
    #[arg(long)]
    project: Option<String>,
    /// Optional restriction to one allowed job channel.
    #[arg(long)]
    channel: Option<String>,
    /// Extracted portable bundle directory containing pinned node_modules. No npm needed.
    #[arg(long)]
    resources: Option<PathBuf>,
    /// System Node >=20 executable; otherwise discover node on PATH/usual install locations.
    #[arg(long)]
    node: Option<PathBuf>,
    /// Optional Chrome/Chromium executable; otherwise use upstream system discovery.
    #[arg(long)]
    browser: Option<PathBuf>,
    /// Save scope/resources and verify preparation without login, network or OS-store access.
    #[arg(long)]
    prepare_only: bool,
    /// Optional Unix timestamp, only for an explicitly timed owner grant.
    #[arg(long)]
    expires_at: Option<u64>,
}

pub fn run(o: Options) -> Result<()> {
    super::validate_expiry(o.expires_at)?;
    let context = LoadContext::current()?;
    let path = setup::path(o.config.as_deref(), &context)?;
    if !path.exists() {
        setup::init(&path)?;
    }
    let source = editor::read(&path)?;
    let mut document = source.parse::<DocumentMut>()?;
    let settings: Settings = loader::decode(document.get("overleaf"))?;
    let old_scope = settings.native_scope.as_ref();
    let scope = Scope {
        expected_email: o
            .expected_email
            .or_else(|| old_scope.map(|s| s.expected_email.clone()))
            .context("--expected-email is required on first login")?
            .to_ascii_lowercase(),
        project: o
            .project
            .or_else(|| old_scope.and_then(|s| s.project.clone())),
        channel: o
            .channel
            .or_else(|| old_scope.and_then(|s| s.channel.clone())),
    };
    scope.validate()?;
    if settings.native_recipient.is_some() {
        ensure!(old_scope.is_none_or(|s| s == &scope),
            "bound native scope cannot change during login; remove/reconfigure the recipient explicitly first");
    }
    let mut login = settings.login.clone().unwrap_or_default();
    let absolute = |p: PathBuf| loader::resolve_path(&p, &std::env::current_dir()?, &context.home);
    if let Some(p) = o.resources {
        login.resources = Some(absolute(p)?);
    }
    if let Some(p) = o.node {
        login.node = Some(absolute(p)?);
    }
    if let Some(p) = o.browser {
        login.browser = Some(absolute(p)?);
    }
    let resources = resources(&login, Some(&path), &context.home)?;
    let node = node(&login)?;
    let helper = prepare(
        &resources,
        path.parent().context("config directory missing")?,
    )?;
    let mut command = Command::new(node);
    // Do this before Node starts: NODE_OPTIONS can execute code before the helper.
    for (name, _) in std::env::vars_os() {
        let key = name.to_string_lossy().to_ascii_uppercase();
        if key.starts_with("NODE_")
            || key.starts_with("OL_")
            || key.starts_with("OVERLEAF_")
            || key.ends_with("PROXY")
            || matches!(key.as_str(), "SSL_CERT_FILE" | "SSL_CERT_DIR")
        {
            command.env_remove(name);
        }
    }
    command
        .arg(helper)
        .arg("--fridica")
        .arg(std::env::current_exe()?)
        .arg("--config")
        .arg(&path)
        .arg("--expected-email")
        .arg(&scope.expected_email);
    if let Some(project) = &scope.project {
        command.arg("--project").arg(project);
    }
    if let Some(browser) = &login.browser {
        command.arg("--browser").arg(browser);
    }
    if let Some(expiry) = o.expires_at {
        command.arg("--expires-at").arg(expiry.to_string());
    }
    // Resource validation has no browser, network, credential-store or backend side effects.
    ensure!(
        command
            .arg("--check")
            .status()
            .context("could not start Node >=20")?
            .success(),
        "Overleaf resources are not ready; supply the pinned portable bundle and Node >=20"
    );
    if document.get("overleaf").is_none() {
        document["overleaf"] = Item::Table(Table::new());
    }
    let mut scope_table = Table::new();
    scope_table["expected_email"] = value(&scope.expected_email);
    if let Some(project) = &scope.project {
        scope_table["project"] = value(project);
    }
    if let Some(channel) = &scope.channel {
        scope_table["channel"] = value(channel);
    }
    document["overleaf"]["native_scope"] = Item::Table(scope_table);
    document["overleaf"]["enabled"] = value(true);
    login.resources = Some(resources);
    let mut login_table = Table::new();
    for (key, path) in [
        ("resources", &login.resources),
        ("node", &login.node),
        ("browser", &login.browser),
    ] {
        if let Some(path) = path {
            login_table[key] = value(path.to_string_lossy().as_ref());
        }
    }
    document["overleaf"]["login"] = Item::Table(login_table);
    let updated = document.to_string();
    if updated != source {
        editor::replace(&path, &editor::fingerprint(&source), &updated)?;
    }
    if o.prepare_only {
        println!("Overleaf account login prepared. Eligible workers attach native tools automatically; existing restrictions are retained.");
        return Ok(());
    }
    // std::process::Command has no remove_arg; use the same sanitized child configuration.
    let args: Vec<_> = command
        .get_args()
        .filter(|a| *a != "--check")
        .map(|a| a.to_owned())
        .collect();
    let mut login_command = Command::new(command.get_program());
    for (key, val) in command.get_envs() {
        match val {
            Some(v) => {
                login_command.env(key, v);
            }
            None => {
                login_command.env_remove(key);
            }
        }
    }
    ensure!(
        login_command.args(args).status()?.success(),
        "Overleaf login did not complete; no listener activation performed"
    );
    Ok(())
}

fn prepare(resources: &Path, parent: &Path) -> Result<PathBuf> {
    let id = format!(
        "{:x}",
        Sha256::digest(format!("{HELPER}\n{}", resources.display()))
    );
    let cache = parent.join("overleaf-login");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&cache)?;
    let target = cache.join(id);
    if !target.exists() {
        let staged = tempfile::Builder::new()
            .prefix(".prepare-")
            .tempdir_in(&cache)?;
        fs::write(staged.path().join("owner-login.mjs"), HELPER)?;
        fs::set_permissions(
            staged.path().join("owner-login.mjs"),
            fs::Permissions::from_mode(0o600),
        )?;
        symlink(
            resources.join("node_modules"),
            staged.path().join("node_modules"),
        )?;
        fs::rename(staged.path(), &target)?;
    }
    ensure!(
        fs::read_to_string(target.join("owner-login.mjs"))? == HELPER
            && fs::read_link(target.join("node_modules"))? == resources.join("node_modules"),
        "prepared login resource mismatch"
    );
    Ok(target.join("owner-login.mjs"))
}
