use fridica::cli::{assets, candidate};
use std::{fs, path::Path};

#[test]
fn service_defaults_to_observer_and_quotes_paths_without_shell_expansion() {
    let unit = candidate::service(
        Path::new("/opt/fridica bin/fridica"),
        Path::new("/home/a/$cfg%/file\".toml"),
        Path::new("/home/a/$env%"),
        false,
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
    assert!(unit.contains("start --config \"/config\" --active\n"));
    assert!(!unit.contains("--observe-only"));
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
fn start_modes_are_exclusive_and_the_record_command_is_gone() {
    for args in [
        vec!["start", "--active", "--observe-only"],
        vec!["start", "--active", "--check-ready"],
        vec!["start", "--active", "--deployment-record", "/nope"],
        vec!["deployment-record", "--output", "/nope"],
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
            .args(&args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2), "{args:?}");
    }
}
