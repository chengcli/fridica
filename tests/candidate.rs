use fridica::{
    cli::{
        assets,
        candidate::{self, Build},
    },
    config::{self, Config, LoadContext},
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

fn fixture(root: &Path) -> Config {
    fs::create_dir(root.join("project")).unwrap();
    let source = format!(
        r#"
[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[machines.local.workspaces]
project="project"
[isolation]
private_files=[{}]
"#,
        serde_json::json!(root.join("deployment.json"))
    );
    config::loader::parse(
        &source,
        &root.join("config.toml"),
        &LoadContext {
            home: root.to_owned(),
            runtime_dir: None,
            uid: users::get_current_uid(),
            protected: vec![],
        },
    )
    .unwrap()
}
fn build() -> Build {
    Build {
        version: "0.4.0-dev.0".into(),
        source_id: "a".repeat(64),
        target: "x86_64-unknown-linux-gnu".into(),
    }
}

#[test]
fn deployment_record_path_through_a_symlinked_directory_matches_resolved_private_files() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    fs::create_dir(&real).unwrap();
    std::os::unix::fs::symlink(&real, temp.path().join("link")).unwrap();
    let config = fixture(&temp.path().join("link"));
    let path = temp.path().join("link/deployment.json");
    candidate::write_attestation(&config, &path, build(), 123.).unwrap();
    candidate::validate(&config, &build(), &path).unwrap();
    let unlisted = temp.path().join("link/other.json");
    assert!(candidate::write_attestation(&config, &unlisted, build(), 123.).is_err());
}

#[test]
fn deployment_record_binds_build_config_host_and_explicit_checks() {
    let temp = tempfile::tempdir().unwrap();
    let config = fixture(temp.path());
    let path = temp.path().join("deployment.json");
    let build = build();
    candidate::write_attestation(&config, &path, build.clone(), 123.).unwrap();
    candidate::validate(&config, &build, &path).unwrap();
    assert!(candidate::write_attestation(&config, &path, build.clone(), 123.).is_err());
    let mut changed = build.clone();
    changed.source_id = "b".repeat(64);
    assert!(candidate::validate(&config, &changed, &path).is_err());
    let mut changed = config.clone();
    changed.fingerprint = "different".into();
    assert!(candidate::validate(&changed, &build, &path).is_err());
    let original = fs::read(&path).unwrap();
    for (key, value) in [
        ("host", serde_json::json!("another-host")),
        ("executable_sha256", serde_json::json!("0".repeat(64))),
        ("target_conformance", serde_json::json!(false)),
        ("recovery_rehearsal", serde_json::json!(false)),
        ("observe_only_reconciliation", serde_json::json!(false)),
    ] {
        let mut data: serde_json::Value = serde_json::from_slice(&original).unwrap();
        data[key] = value;
        fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        assert!(
            candidate::validate(&config, &build, &path).is_err(),
            "{key}"
        );
    }
    fs::write(&path, &original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(candidate::validate(&config, &build, &path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(candidate::validate(&config, &build, &link).is_err());
    fs::hard_link(&path, temp.path().join("hardlink")).unwrap();
    assert!(candidate::validate(&config, &build, &path).is_err());
}

#[test]
fn unpackaged_or_unprotected_records_cannot_enable_active_start() {
    let temp = tempfile::tempdir().unwrap();
    let mut config = fixture(temp.path());
    let mut unpackaged = build();
    unpackaged.source_id = "unpackaged".into();
    assert!(candidate::attest(&config, unpackaged, 123.).is_err());
    assert!(candidate::attest(&config, build(), f64::NAN).is_err());
    config.isolation.private_files.clear();
    assert!(candidate::write_attestation(
        &config,
        &temp.path().join("deployment.json"),
        build(),
        123.
    )
    .is_err());
}

#[test]
fn service_defaults_to_observer_and_quotes_paths_without_shell_expansion() {
    let unit = candidate::service(
        Path::new("/opt/fridica bin/fridica"),
        Path::new("/home/a/$cfg%/file\".toml"),
        Path::new("/home/a/$env%"),
        None,
    )
    .unwrap();
    assert!(unit.contains("--observe-only"));
    assert!(!unit.contains("--active"));
    assert!(unit.contains("\"/home/a/$$cfg%%/file\\\".toml\""));
    assert!(unit.contains("EnvironmentFile=\"/home/a/$env%%\""));
    assert!(unit.contains("KillMode=control-group"));
    assert!(candidate::service(
        Path::new("relative"),
        Path::new("/config"),
        Path::new("/env"),
        None
    )
    .is_err());
    assert!(candidate::service(
        Path::new("/exe"),
        Path::new("/config\nExecStart=evil"),
        Path::new("/env"),
        None
    )
    .is_err());
    let unit = candidate::service(
        Path::new("/exe"),
        Path::new("/config"),
        Path::new("/env"),
        Some(Path::new("/record")),
    )
    .unwrap();
    assert!(unit.contains("--active --deployment-record \"/record\""));
}

#[test]
fn exported_assets_are_complete_and_never_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("assets");
    assets::export(&path).unwrap();
    for (name, data) in assets::catalog() {
        assert_eq!(fs::read(path.join(name)).unwrap(), data);
    }
    assert!(assets::export(&path).is_err());
    let helpers = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/exec");
    for entry in fs::read_dir(helpers).unwrap() {
        let file = entry.unwrap().path();
        if file.extension().is_some_and(|e| e == "py") {
            assert!(
                path.join("helpers")
                    .join(file.file_name().unwrap())
                    .exists(),
                "{}",
                file.display()
            );
        }
    }
    for name in ["result-format.txt", "result-schema.json"] {
        assert_eq!(
            fs::read(path.join("workers").join(name)).unwrap(),
            fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/workers")
                    .join(name)
            )
            .unwrap()
        );
    }
    for i in 1..=6 {
        assert!(path.join(format!("migrations/{i:03}.sql")).exists());
    }
}

#[test]
fn active_cli_requires_all_flags_before_state_or_credentials() {
    for args in [
        vec!["start", "--active"],
        vec![
            "start",
            "--active",
            "--observe-only",
            "--deployment-record",
            "/nope",
        ],
        vec![
            "deployment-record",
            "--config",
            "/nope",
            "--output",
            "/nope",
        ],
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
            .args(args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
    }
}
