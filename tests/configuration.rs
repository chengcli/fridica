use fridica::config::{self, loader, LoadContext};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn context(root: &Path) -> LoadContext {
    LoadContext {
        home: root.join("home"),
        runtime_dir: None,
        uid: 123,
        protected: vec![],
    }
}
fn setup(root: &Path) {
    for name in ["project", "etc", "home"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }
    std::fs::write(root.join("etc/repos.toml"), "# fixture\n").unwrap();
    std::fs::write(root.join("etc/contract-custom.md"), "Fixture contract\n").unwrap();
}
fn normalize(v: &mut Value, root: &Path, home: &Path) {
    match v {
        Value::Object(m) => {
            m.remove("attention");
            m.remove("fingerprint");
            for v in m.values_mut() {
                normalize(v, root, home);
            }
        }
        Value::Array(a) => {
            for v in a {
                normalize(v, root, home);
            }
        }
        Value::String(s) => {
            *s = s
                .replace(home.to_str().unwrap(), "__HOME__")
                .replace(root.to_str().unwrap(), "__ROOT__");
        }
        // Python numeric equality does not distinguish a resource's 128 from 128.0.
        Value::Number(n) => {
            *n = serde_json::Number::from_f64(n.as_f64().unwrap()).unwrap();
        }
        _ => {}
    }
}
#[test]
fn frozen_configuration_resolution_and_validation_match() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("corpus/config.json")).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("fc-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = directory.path().canonicalize().unwrap();
    setup(&root);
    let context = context(&root);
    for case in cases {
        let source = case["source"]
            .as_str()
            .unwrap()
            .replace("__ROOT__", root.to_str().unwrap());
        let path = root.join("etc/config.toml");
        std::fs::write(&path, &source).unwrap();
        let actual = config::load(&path, &context);
        if case["error"] == true {
            assert!(actual.is_err(), "accepted invalid fixture {}", case["id"]);
            continue;
        }
        let actual = actual.unwrap_or_else(|e| panic!("{}: {e:#}", case["id"]));
        assert_eq!(
            actual.fingerprint,
            format!("{:x}", Sha256::digest(source.as_bytes()))
        );
        let mut actual = serde_json::to_value(actual).unwrap();
        let mut expected = case["expected"].clone();
        normalize(&mut actual, &root, &context.home);
        normalize(&mut expected, &root, &context.home);
        assert_eq!(actual, expected, "fixture {}", case["id"]);
    }
}
fn basic(root: &Path) -> String {
    format!("[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[machines.local.workspaces]\nproject='{}'\n[state]\npath='{}'\n",root.join("project").display(),root.join("db").display())
}
#[test]
fn v6_attention_defaults_and_legacy_overrides_do_not_mutate_configuration() {
    let dir = tempfile::Builder::new()
        .prefix("fc-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path();
    setup(root);
    let context = context(root);
    let path = root.join("etc/config.toml");
    let source = format!(
        "{}\n[limits]\nmax_wait_replies=9\nmax_no_progress=2\n",
        basic(root)
    );
    std::fs::write(&path, &source).unwrap();
    let config = config::load(&path, &context).unwrap();
    assert_eq!(
        (
            config.attention.streak_signal,
            config.attention.max_echo_replies_per_hour,
            config.attention.max_replies_per_hour
        ),
        (2, 6, 20)
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    let explicit = format!("{source}\n[attention]\nstreak_signal=7\nmention_grace=60\n");
    assert_eq!(
        loader::parse(&explicit, &path, &context)
            .unwrap()
            .attention
            .streak_signal,
        7
    );
    for invalid in [
        "mention_grace=0",
        "max_replies_per_hour=0",
        "streak_signal=true",
        "surprise=3",
    ] {
        assert!(loader::parse(
            &format!("{source}\n[attention]\n{invalid}"),
            &path,
            &context
        )
        .is_err());
    }
}
#[test]
fn paths_protect_state_control_rules_and_executables_including_unicode_aliases() {
    let dir = tempfile::Builder::new()
        .prefix("fc-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path();
    setup(root);
    let mut context = context(root);
    let path = root.join("etc/config.toml");
    let source = basic(root);
    context.protected.push(root.join("project/fridica"));
    assert!(loader::parse(&source, &path, &context)
        .unwrap_err()
        .to_string()
        .contains("inside writable"));
    context.protected.clear();
    let socket = format!(
        "{source}control_socket='{}'\n",
        root.join("project/control.sock").display()
    );
    assert!(loader::parse(&socket, &path, &context)
        .unwrap_err()
        .to_string()
        .contains("control_socket must be outside"));
    assert!(loader::parse(&source, &root.join("project/config.toml"), &context).is_err());
    std::fs::create_dir(root.join("Straße")).unwrap();
    let folded = source
        .replace(
            root.join("project").to_str().unwrap(),
            root.join("Straße").to_str().unwrap(),
        )
        .replace(
            root.join("db").to_str().unwrap(),
            root.join("STRASSE/db").to_str().unwrap(),
        );
    assert!(loader::parse(&folded, &path, &context)
        .unwrap_err()
        .to_string()
        .contains("state.path must be outside"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("project"), root.join("alias")).unwrap();
        let symlinked = source.replace(
            root.join("db").to_str().unwrap(),
            root.join("alias/db").to_str().unwrap(),
        );
        assert!(loader::parse(&symlinked, &path, &context).is_err());
    }
    let readonly = source.replace(
        &format!("project='{}'", root.join("project").display()),
        &format!(
            "project={{path='{}',policy={{mode='read-only'}}}}",
            root.join("project").display()
        ),
    );
    assert!(loader::parse(&readonly, &root.join("project/config.toml"), &context).is_ok());
}
#[test]
fn long_socket_paths_fall_back_and_token_validation_never_discloses_values() {
    let dir = tempfile::Builder::new()
        .prefix("fc-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path();
    setup(root);
    let context = context(root);
    let path = root.join("etc/config.toml");
    let source = basic(root).replace(
        root.join("db").to_str().unwrap(),
        root.join("x".repeat(110)).join("db").to_str().unwrap(),
    );
    let config = loader::parse(&source, &path, &context).unwrap();
    assert!(config.state.control_socket.starts_with("/tmp/fridica-123"));
    assert!(config.state.control_socket.as_os_str().len() <= 100);
    let error = config
        .validate_tokens(|name| {
            Some(
                if name == "SLACK_APP_TOKEN" {
                    "xapp-test"
                } else {
                    "secret-wrong-user-token"
                }
                .into(),
            )
        })
        .unwrap_err();
    assert!(error.to_string().contains("SLACK_USER_TOKEN"));
    assert!(!error.to_string().contains("secret-wrong"));
    config
        .validate_tokens(|name| {
            Some(
                if name == "SLACK_APP_TOKEN" {
                    "xapp-test"
                } else {
                    "xoxp-test"
                }
                .into(),
            )
        })
        .unwrap();
    let source = format!(
        "{source}control_socket='{}'",
        root.join("x".repeat(110)).display()
    );
    assert!(loader::parse(&source, &path, &context).is_err());
}
#[test]
fn paths_resolve_symlinks_before_parent_components() {
    let dir = tempfile::Builder::new()
        .prefix("fc-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path();
    setup(root);
    #[cfg(unix)]
    {
        std::fs::create_dir_all(root.join("real/deep")).unwrap();
        std::os::unix::fs::symlink(root.join("real/deep"), root.join("link")).unwrap();
        assert_eq!(
            loader::resolve_path(Path::new("link/../new/db"), root, &root.join("home")).unwrap(),
            root.canonicalize().unwrap().join("real/new/db")
        );
    }
    assert_eq!(
        loader::resolve_path(Path::new("~/new"), root, &root.join("home")).unwrap(),
        root.canonicalize().unwrap().join("home/new")
    );
    assert!(loader::resolve_path(&PathBuf::from(""), root, &root.join("home")).is_err());
}

#[test]
fn offline_cli_checks_configuration_without_creating_state_or_reading_tokens() {
    let dir = tempfile::Builder::new()
        .prefix("fc-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path();
    setup(root);
    let path = root.join("etc/config.toml");
    std::fs::write(&path, basic(root)).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args(["check-config", "--config"])
        .arg(&path)
        .env_remove("SLACK_APP_TOKEN")
        .env_remove("SLACK_USER_TOKEN")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["machines"], serde_json::json!(["local"]));
    assert_eq!(result["attention"]["max_replies_per_hour"], 20);
    assert!(!root.join("db").exists());
    assert!(!root.join("control.sock").exists());
}

#[test]
fn named_user_home_paths_keep_python_expansion_semantics() {
    use users::os::unix::UserExt;
    let user = users::get_user_by_uid(users::get_current_uid()).unwrap();
    let path = PathBuf::from(format!(
        "~{}/fridica-test-nonexistent",
        user.name().to_str().unwrap()
    ));
    let actual = loader::resolve_path(&path, Path::new("/tmp"), Path::new("/not-used")).unwrap();
    let expected = loader::resolve_path(
        &user.home_dir().join("fridica-test-nonexistent"),
        Path::new("/tmp"),
        Path::new("/not-used"),
    )
    .unwrap();
    assert_eq!(actual, expected);
    assert!(loader::resolve_path(
        Path::new("~fridica-nonexistent-test-account-98765/x"),
        Path::new("/tmp"),
        Path::new("/not-used")
    )
    .is_err());
}

#[test]
fn path_comparison_covers_every_python_casefold_mapping() {
    let folds: Vec<Value> = serde_json::from_str(include_str!("corpus/path_folds.json")).unwrap();
    for row in folds {
        let input = row["input"].as_str().unwrap();
        assert_eq!(
            loader::casefold_path(Path::new(input)),
            PathBuf::from(row["expected"].as_str().unwrap()),
            "{input:?}"
        );
    }
    assert_eq!(
        loader::casefold_path(Path::new("/École/STRASSE")),
        loader::casefold_path(Path::new("/e\u{301}cole/Straße"))
    );
}

fn remote_machine() -> &'static str {
    "\n[machines.remote]\ntransport='ssh'\nhost='owner@target'\n[machines.remote.resources]\ngpus=[0]\n[machines.remote.policy]\ngpu_confine=true\n[machines.remote.workspaces]\nproject='~/project'\n"
}

#[test]
fn isolation_inventory_resolves_only_local_paths_and_preserves_source_and_legacy_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    setup(root);
    let path = root.join("etc/config.toml");
    let source = format!("{}{}\n# Owner-provisioned, no tokens stored here.\n[isolation]\nprivate_files=['keys/control.key']\nmcp_aliases=['opaque-wrapper']\nmcp_urls=['http://localhost:8123/fridica']\n[isolation.remote.remote]\nhost='owner@target'\nprivate_files=['~/.local/state/fridica/control.sock','/shared/private/state.db']\n",basic(root),remote_machine());
    std::fs::write(&path, &source).unwrap();
    let parsed = config::load(&path, &context(root)).unwrap();
    assert_eq!(
        parsed.isolation.private_files,
        vec![root.join("etc/keys/control.key")]
    );
    assert_eq!(
        parsed.isolation.remote["remote"].private_files[0],
        "~/.local/state/fridica/control.sock"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    assert!(!root.join("db").exists());
    assert!(!root.join("etc/keys").exists());
    let legacy = loader::parse(&basic(root), &path, &context(root)).unwrap();
    assert!(legacy.isolation.is_empty());
    assert!(serde_json::to_value(&legacy)
        .unwrap()
        .get("isolation")
        .is_none());
    assert_ne!(parsed.fingerprint, legacy.fingerprint);
    let mut moved = parsed.clone();
    moved
        .machines
        .machines
        .iter_mut()
        .find(|m| m.name == "remote")
        .unwrap()
        .host = "other-host".into();
    assert!(fridica::exec::isolation::Isolation::new(&moved, &[]).is_err());
}

#[test]
fn isolation_configuration_rejects_wrong_hosts_unsafe_paths_and_embedded_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    setup(root);
    let base = format!("{}{}", basic(root), remote_machine());
    loader::parse(&base, &root.join("etc/config.toml"), &context(root)).unwrap();
    for fragment in [
        "[isolation]\nunknown=true",
        "[isolation]\nmcp_urls=['http://user:private-secret@host/']",
        "[isolation]\nmcp_urls=['https://host/?key=private-secret']",
        "[isolation]\nmcp_urls=['http://']",
        "[isolation]\nmcp_urls=['http:host']",
        "[isolation]\nprivate_files=['/private-secret']",
        "[isolation.remote.remote]\nhost='changed-host'\nprivate_files=['~/private/secret']",
        "[isolation.remote.remote]\nhost='owner@target'\nprivate_files=[]",
        "[isolation.remote.remote]\nhost='owner@target'\nprivate_files=['~/../private/secret']",
        "[isolation.remote.remote]\nhost='owner@target'\nprivate_files=['relative/secret']",
        "[isolation.remote.remote]\nhost='owner@target'\nprivate_files=['/secret']",
        "[isolation.remote.remote]\nhost='owner@target'\nprivate_files=['~/private/secret']\nunknown=true",
        "[isolation.remote.missing]\nhost='owner@target'\nprivate_files=['~/private/secret']",
        "[isolation.remote.local]\nhost='owner@target'\nprivate_files=['~/private/secret']",
    ] {
        let error = loader::parse(&format!("{base}\n{fragment}\n"), &root.join("etc/config.toml"), &context(root)).unwrap_err();
        assert!(!format!("{error:#}").contains("private-secret"));
    }
    // Private files cannot be exposed even through a read-only local workspace.
    let source = format!(
        "{}\n[policy]\nmode='read-only'\n[isolation]\nprivate_files=['../project/control.key']\n",
        basic(root)
    );
    assert!(loader::parse(&source, &root.join("etc/config.toml"), &context(root)).is_err());
}

#[test]
fn offline_cli_reports_missing_and_configured_inventory_without_probe_or_private_values() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    setup(root);
    let path = root.join("etc/config.toml");
    let base = format!("{}{}", basic(root), remote_machine());
    for configured in [false, true] {
        let inventory = if configured {
            "\n[isolation]\nmcp_aliases=['private-alias']\n[isolation.remote.remote]\nhost='owner@target'\nprivate_files=['~/private/hidden-capability.key']\n"
        } else {
            ""
        };
        std::fs::write(&path, format!("{base}{inventory}")).unwrap();
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
            .args(["check-config", "--config"])
            .arg(&path)
            .env_clear()
            .env("HOME", root.join("home"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let report: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            report["isolation"]["remote"][0]["inventory"],
            if configured { "configured" } else { "missing" }
        );
        assert_eq!(report["isolation"]["runtime_checks"], "not_run");
        assert!(
            !text.contains("private-alias")
                && !text.contains("hidden-capability")
                && !text.contains("owner@target")
        );
        assert!(!root.join("db").exists());
        assert!(!root.join("home/.codex").exists());
    }
}
