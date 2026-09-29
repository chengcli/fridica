//! Initial setup deliberately accepts incomplete configuration. It edits only
//! Slack identity fields and never opens state or certifies launch readiness.
use super::{editor, loader, LoadContext};
use anyhow::{bail, Context, Result};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use toml_edit::{Array, DocumentMut, Item, Table, Value};

pub const TEMPLATE: &str = include_str!("template.toml");
pub const MANIFEST: &str = include_str!("../../slack/manifest.yaml");
pub fn path(requested: Option<&Path>, context: &LoadContext) -> Result<PathBuf> {
    loader::resolve_path(
        requested.unwrap_or(&context.home.join(".config/fridica/config.toml")),
        &std::env::current_dir()?,
        &context.home,
    )
}
fn create(path: &Path, source: &str, preserve_existing: bool) -> Result<()> {
    let mut file = tempfile::Builder::new()
        .prefix(".fridica-init-")
        .tempfile_in(path.parent().context("setup path has no directory")?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(source.as_bytes())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(path) {
        Ok(_) => (),
        Err(error)
            if preserve_existing && error.error.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            return Ok(())
        }
        Err(error) => return Err(error.into()),
    }
    editor::sync_directory(path)
}
pub fn init(path: &Path) -> Result<()> {
    if path
        .file_name()
        .is_some_and(|name| name == "contract.md" || name == "manifest.yaml")
    {
        bail!("configuration filename conflicts with a companion asset");
    }
    if fs::symlink_metadata(path).is_ok() {
        bail!("configuration already exists; nothing changed");
    }
    let parent = path.parent().context("setup path has no directory")?;
    let mut directory = fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(parent)?;
    // Publish config last. A failed attempt can be retried while preserving
    // any owner files or already completed companion assets.
    for (name, content) in [
        ("contract.md", super::contract::DEFAULT),
        ("manifest.yaml", MANIFEST),
    ] {
        let target = parent.join(name);
        if fs::symlink_metadata(&target).is_err() {
            create(&target, content, true)?;
        }
    }
    create(path, TEMPLATE, false)
}
#[derive(Default, Clone)]
pub struct Identity {
    pub owner: Option<String>,
    pub workspace: Option<String>,
    pub channels: Vec<String>,
}
impl Identity {
    pub fn validate(&self) -> Result<()> {
        if self
            .owner
            .as_ref()
            .is_some_and(|s| !loader::slack_id(s, "UW"))
        {
            bail!("--owner-id must be a Slack member ID");
        }
        if self
            .workspace
            .as_ref()
            .is_some_and(|s| !loader::slack_id(s, "T"))
        {
            bail!("--workspace-id must be a Slack team ID");
        }
        if self.channels.iter().any(|s| !loader::slack_id(s, "CG")) {
            bail!("--channel-id values must be Slack channel IDs");
        }
        if self.owner.is_none() && self.workspace.is_none() && self.channels.is_empty() {
            bail!("nothing to change; pass --detect or IDs");
        }
        Ok(())
    }
}
/// Snapshot before discovery/selection: a changed file must not be overwritten.
pub struct Draft {
    path: PathBuf,
    document: DocumentMut,
    fingerprint: String,
}
impl Draft {
    pub fn open(path: PathBuf) -> Result<Self> {
        let source = editor::read(&path)?;
        let document = source
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid TOML configuration"))?;
        Ok(Self {
            path,
            document,
            fingerprint: editor::fingerprint(&source),
        })
    }
    pub fn token_variable(&self) -> Result<String> {
        let variable = match self.document.get("slack") {
            None => "SLACK_USER_TOKEN",
            Some(table) => match table
                .as_table_like()
                .context("slack must be a table")?
                .get("user_token_env")
            {
                None => "SLACK_USER_TOKEN",
                Some(item) => item
                    .as_str()
                    .context("slack.user_token_env must name an environment variable")?,
            },
        };
        if !loader::env_name(variable) {
            bail!("slack.user_token_env must name an environment variable");
        }
        Ok(variable.into())
    }
    pub fn apply(mut self, identity: Identity) -> Result<()> {
        identity.validate()?;
        if let Some(owner) = identity.owner {
            self.set("owner", "slack_user", owner.into())?;
        }
        if let Some(workspace) = identity.workspace {
            self.set("slack", "workspace", workspace.into())?;
        }
        if !identity.channels.is_empty() {
            let mut unique = Vec::new();
            for id in identity.channels {
                if !unique.contains(&id) {
                    unique.push(id);
                }
            }
            let channels: Array = unique.iter().map(String::as_str).collect();
            self.set("slack", "channels", Value::Array(channels))?;
        }
        editor::replace(&self.path, &self.fingerprint, &self.document.to_string())
    }
    fn set(&mut self, section: &str, key: &str, mut value: Value) -> Result<()> {
        if self.document.get(section).is_none() {
            self.document[section] = Item::Table(Table::new());
        }
        let table = self.document[section]
            .as_table_like_mut()
            .context("identity section must be a table")?;
        if let Some(old) = table.get(key).and_then(Item::as_value) {
            *value.decor_mut() = old.decor().clone();
        }
        table.insert(key, Item::Value(value));
        Ok(())
    }
}
