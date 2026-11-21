use fridica::cli::{assets, candidate};
use std::{fs, path::Path};

#[test]
fn service_defaults_to_active_daemon_and_quotes_paths_without_shell_expansion() {
    let unit = candidate::service(
        Path::new("/opt/fridica bin/fridica"),
        Path::new("/home/a/$cfg%/file\".toml"),
        Path::new("/home/a/$env%"),
        false,
    )
    .unwrap();
    assert!(unit.contains("start --config \"/home/a/$$cfg%%/file\\\".toml\"\n"));
    assert!(!unit.contains("--observe-only") && !unit.contains("--active"));
    assert!(unit.contains("\"/home/a/$$cfg%%/file\\\".toml\""));
    assert!(unit.contains("EnvironmentFile=\"/home/a/$env%%\""));
    assert!(unit.contains("KillMode=control-group"));
    assert!(candidate::service(
        Path::new("relative"),
        Path::new("/config"),
        Path::new("/env"),
        false
    )
    .is_err());
    assert!(candidate::service(
        Path::new("/exe"),
        Path::new("/config\nExecStart=evil"),
        Path::new("/env"),
        false
    )
    .is_err());
    let unit = candidate::service(
        Path::new("/exe"),
        Path::new("/config"),
        Path::new("/env"),
        true,
    )
    .unwrap();
    assert!(unit.contains("start --config \"/config\" --observe-only\n"));
}

#[test]
fn service_print_wraps_daemon_with_summon() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args([
            "service-print",
            "--config",
            "/cfg",
            "--summon-file",
            "/private/a $b%.yml",
            "--summon-provider",
            "/owner/Fridica Tools/fridica-secret",
            "--observe-only",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let unit = String::from_utf8(output.stdout).unwrap();
    assert!(unit.contains(r#"ExecStart=summon --provider "/owner/Fridica Tools/fridica-secret" -f "/private/a $$b%%.yml" "#));
    assert!(unit.contains(" start --config \"/cfg\" --observe-only\n"));
    assert!(!unit.contains("EnvironmentFile="));
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
    let dashboard = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/dashboard");
    for entry in fs::read_dir(dashboard).unwrap() {
        let file = entry.unwrap().path();
        assert_eq!(
            fs::read(path.join("dashboard").join(file.file_name().unwrap())).ok(),
            fs::read(&file).ok(),
            "{}",
            file.display()
        );
    }
    for (name, expected) in [
        ("result-format.txt", fridica::workers::result::FORMAT_NOTE),
        ("result-schema.json", fridica::workers::result::SCHEMA_JSON),
    ] {
        assert_eq!(
            fs::read_to_string(path.join("workers").join(name)).unwrap(),
            expected,
            "{name}"
        );
    }
    for i in 1..=fridica::store::schema::VERSION {
        assert!(path.join(format!("migrations/{i:03}.sql")).exists());
    }
}

#[test]
fn start_modes_are_exclusive_and_removed_flags_are_rejected() {
    for args in [
        vec!["start", "--observe-only", "--check-ready"],
        vec!["start", "--active"],
        vec!["start", "--deployment-record", "/nope"],
        vec!["service-print", "--environment-file", "/env", "--active"],
        vec![
            "service-print",
            "--environment-file",
            "/env",
            "--summon-file",
            "/map",
        ],
        vec!["deployment-record", "--output", "/nope"],
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2), "{args:?}");
    }
}

#[test]
fn native_service_needs_only_the_config() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args(["service-print", "--config", "/cfg"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let unit = String::from_utf8(output.stdout).unwrap();
    assert!(unit.contains(" start --config \"/cfg\""));
    assert!(!unit.contains("summon") && !unit.contains("EnvironmentFile="));
}
