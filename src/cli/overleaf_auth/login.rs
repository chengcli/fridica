//! Standalone owner login using the built-in Rust backend. Preparation never reads credentials.
use crate::config::{
    editor, loader,
    overleaf::{Scope, Settings},
    setup, LoadContext,
};
use crate::core::secret::SecretStore;
use crate::secrets::NativeStore;
use anyhow::{ensure, Context, Result};
use clap::Args;
use fridica_agent::overleaf::login::{
    capture_session_at, normalize_base_url, validate_cookie_at, Identity, ValidationError,
};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use toml_edit::{value, DocumentMut, Item, Table};

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
    /// Deprecated: Rust login needs no separate resource bundle.
    #[arg(long = "resources", hide = true)]
    _resources: Option<PathBuf>,
    /// Deprecated: Rust login needs no Node executable.
    #[arg(long = "node", hide = true)]
    _node: Option<PathBuf>,
    /// Optional Chrome/Chromium executable; otherwise discover an installed browser.
    #[arg(long)]
    browser: Option<PathBuf>,
    /// Overleaf origin, including self-hosted CE (HTTPS; HTTP only for loopback).
    #[arg(long)]
    base_url: Option<String>,
    /// Save scope/browser preferences and verify Rust readiness without login or credential access.
    #[arg(long)]
    prepare_only: bool,
    /// Optional Unix timestamp, only for an explicitly timed owner grant.
    #[arg(long)]
    expires_at: Option<u64>,
}

// Never derive Debug: this is a credential envelope already checked by the config boundary.
#[derive(Deserialize)]
struct Saved {
    cookie: String,
    account_id: String,
    expected_email: String,
    #[serde(default = "default_origin")]
    base_url: String,
}

fn default_origin() -> String {
    "https://www.overleaf.com".into()
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
    let base_url = normalize_base_url(
        o.base_url
            .as_deref()
            .or_else(|| {
                settings
                    .login
                    .as_ref()
                    .and_then(|login| login.base_url.as_deref())
            })
            .unwrap_or("https://www.overleaf.com"),
    )?;
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
    let browser = match o.browser {
        Some(browser) => Some(loader::resolve_path(
            &browser,
            &std::env::current_dir()?,
            &context.home,
        )?),
        None => settings
            .login
            .as_ref()
            .and_then(|login| login.browser.as_ref())
            .map(|browser| {
                loader::resolve_path(
                    browser,
                    path.parent().context("config directory missing")?,
                    &context.home,
                )
            })
            .transpose()?,
    };
    if o.prepare_only {
        save_config(
            &path,
            &source,
            &mut document,
            &scope,
            browser.as_deref(),
            &base_url,
        )?;
        println!("Overleaf login is ready in Rust. Scope and browser preferences saved; existing restrictions are retained.");
        return Ok(());
    }

    // Store failure, malformed grants and grant/account mismatch are errors, never missing login.
    let previous = NativeStore.get(super::ENTRY)?;
    let saved = previous
        .as_deref()
        .map(|previous| {
            let raw = std::str::from_utf8(previous)
                .map_err(|_| anyhow::anyhow!("invalid native session encoding"))?;
            crate::config::overleaf::validate_session_scope(raw, &scope)?;
            let saved: Saved = serde_json::from_str(raw)
                .map_err(|_| anyhow::anyhow!("invalid saved native session"))?;
            ensure!(
                normalize_base_url(&saved.base_url)? == base_url,
                "saved Overleaf origin changed; explicit revocation required"
            );
            ensure!(
                saved.expected_email == scope.expected_email,
                "saved native account changed; explicit revocation required"
            );
            Ok::<_, anyhow::Error>(saved)
        })
        .transpose()?;
    // A restricted saved login must not probe the account dashboard on renewal.
    let mut verification_scope = scope.clone();
    if verification_scope.project.is_none() {
        if let Some(previous) = &previous {
            let raw = std::str::from_utf8(previous)
                .map_err(|_| anyhow::anyhow!("invalid saved session encoding"))?;
            let (_, projects, _) = crate::config::overleaf::session_policy(raw)?;
            if let Some(projects) = projects {
                verification_scope.project = Some(projects.first()
                    .context("saved session has an empty project grant; explicit revocation required")?.clone());
            }
        }
    }
    let (cookie, identity) = match &saved {
        Some(saved) => match validate_cookie_at(
            &base_url,
            &saved.cookie,
            verification_scope.project.as_deref(),
        ) {
            Ok(identity) => (saved.cookie.clone(), identity),
            Err(ValidationError::Expired) => {
                capture(&verification_scope, browser.as_deref(), &base_url)?
            }
            Err(error) => return Err(error.into()),
        },
        None => capture(&verification_scope, browser.as_deref(), &base_url)?,
    };
    ensure!(
        identity.email == scope.expected_email,
        "verified Overleaf account does not match the expected email"
    );
    if let Some(saved) = &saved {
        ensure!(
            identity.user_id == saved.account_id,
            "verified Overleaf account changed; explicit revocation required"
        );
    }
    let mut raw = serde_json::json!({
        "schema":4,
        "base_url":base_url,
        "cookie":cookie,
        "account_id":identity.user_id,
        "expected_email":scope.expected_email,
        "projects":scope.project.as_ref().map(|project| vec![project]),
        "operations":full_operations(),
        "expires_at":o.expires_at,
        "recipient":null,
        "channel":scope.channel,
    })
    .to_string();
    if let Some(previous) = &previous {
        raw = crate::config::overleaf::renew_session(previous, &raw)?;
    }
    crate::config::overleaf::validate_session_scope(&raw, &scope)?;
    save_config(
        &path,
        &source,
        &mut document,
        &scope,
        browser.as_deref(),
        &base_url,
    )?;
    NativeStore.set(super::ENTRY, raw.as_bytes())?;
    println!("Verified Overleaf session saved. Existing grants retained; no listener activated.");
    Ok(())
}

fn capture(scope: &Scope, browser: Option<&Path>, base_url: &str) -> Result<(String, Identity)> {
    println!("Complete Overleaf sign-in in the isolated Chrome or Chromium window.");
    let session = capture_session_at(
        base_url,
        &scope.expected_email,
        scope.project.as_deref(),
        browser,
        Duration::from_secs(300),
    )?;
    Ok((session.cookie, session.identity))
}

fn save_config(
    path: &Path,
    source: &str,
    document: &mut DocumentMut,
    scope: &Scope,
    browser: Option<&Path>,
    base_url: &str,
) -> Result<()> {
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
    let mut login_table = Table::new();
    login_table["base_url"] = value(base_url);
    if let Some(browser) = browser {
        login_table["browser"] = value(browser.to_string_lossy().as_ref());
    }
    document["overleaf"]["login"] = Item::Table(login_table);
    let updated = document.to_string();
    if updated != source {
        editor::replace(path, &editor::fingerprint(source), &updated)?;
    }
    Ok(())
}

fn full_operations() -> Vec<String> {
    let mut ops = fridica_agent::overleaf::native_operations();
    ops.push("direct_edit".into());
    ops
}
