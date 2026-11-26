//! Inventory the exact embedded payload used by the standalone candidate.
use anyhow::Result;
use std::{fs, os::unix::fs::DirBuilderExt, path::Path};

pub fn catalog() -> Vec<(String, &'static [u8])> {
    macro_rules! assets {
        ($($name:literal => $path:literal),* $(,)?) => {vec![$(($name.to_owned(), include_bytes!($path).as_slice())),*]};
    }
    let mut entries = vec![
        (
            "workers/result-format.txt".to_owned(),
            crate::workers::result::FORMAT_NOTE.as_bytes(),
        ),
        (
            "workers/result-schema.json".to_owned(),
            crate::workers::result::SCHEMA_JSON.as_bytes(),
        ),
    ];
    entries.extend(assets![
        "contract.md" => "../../assets/contract.md",
        "template.toml" => "../config/template.toml",
        "secrets.yml" => "../../assets/secrets.yml",
        "manifest.yaml" => "../../assets/slack-manifest.yaml",
        "prompts/parent-action.md" => "../../assets/prompts/parent-action.md",
        "prompts/parent-triage.md" => "../../assets/prompts/parent-triage.md",
        "prompts/untrusted.md" => "../../assets/prompts/untrusted.md",
        "helpers/settings_paths.py" => "../exec/settings_paths.py",
        "helpers/artifact_read.py" => "../exec/artifact_read.py",
        "helpers/cleanup.py" => "../exec/cleanup.py",
        "helpers/credentials.py" => "../exec/credentials.py",
        "helpers/isolation_bootstrap.py" => "../exec/isolation_bootstrap.py",
        "helpers/fetch_helper.py" => "../exec/fetch_helper.py",
        "helpers/isolation_settings.py" => "../exec/isolation_settings.py",
        "helpers/isolation_helper.py" => "../exec/isolation_helper.py",
        "helpers/mcp_startup.py" => "../exec/mcp_startup.py",
        "helpers/overleaf_git.py" => "../exec/overleaf_git.py",
        "helpers/readiness.py" => "../exec/readiness.py",
        "helpers/diagnostics.py" => "../exec/diagnostics.py",
    ]);
    // Whatever the dashboard serves, so a new page asset cannot be left out.
    entries.extend(crate::dashboard::FILES.iter().map(|(path, _, data)| {
        let name = if *path == "/" { "/index.html" } else { path };
        (format!("dashboard{name}"), *data)
    }));
    entries.extend(
        crate::config::provisions::FILES
            .iter()
            .map(|(name, body)| (format!("provisions/{name}"), body.as_bytes())),
    );
    entries.extend(
        crate::store::schema::MIGRATIONS
            .iter()
            .enumerate()
            .map(|(i, sql)| (format!("migrations/{:03}.sql", i + 1), sql.as_bytes())),
    );
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Refuse existing destinations, including symlinks; never overwrite configuration.
pub fn export(path: &Path) -> Result<()> {
    // Reserve destination without following symlinks; a partial failed export is
    // conspicuous and cannot be mistaken for an existing verified installation.
    fs::DirBuilder::new().mode(0o700).create(path)?;
    for (name, data) in catalog() {
        let target = path.join(name);
        fs::create_dir_all(target.parent().unwrap())?;
        fs::write(target, data)?;
    }
    Ok(())
}
