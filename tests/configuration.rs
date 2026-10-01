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
        (2, 10, 20)
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
fn offline_cli_defaults_to_the_home_configuration_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = &dir.path().canonicalize().unwrap();
    setup(root);
    let default = root.join("home/.config/fridica/config.toml");
    std::fs::create_dir_all(default.parent().unwrap()).unwrap();
    std::fs::write(&default, basic(root)).unwrap();
    let check = |explicit: bool| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"));
        command.arg("check-config").env("HOME", root.join("home"));
        if explicit {
            command.arg("--config").arg(&default);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["fingerprint"].clone()
    };
    assert_eq!(check(false), check(true));
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
    // Resolved paths are canonical (macOS temp dirs live under /private/var).
    let root = &dir.path().canonicalize().unwrap();
    setup(root);
    let path = root.join("etc/config.toml");
    let source = format!("{}{}\n# Owner-provisioned, no tokens stored here.\n[isolation]\nmcp_aliases=['opaque-wrapper']\nmcp_urls=['http://localhost:8123/fridica']\n[isolation.remote.remote]\nhost='owner@target'\nmcp_inventory_complete=true\n",basic(root),remote_machine());
    std::fs::write(&path, &source).unwrap();
    let parsed = config::load(&path, &context(root)).unwrap();
    assert_eq!(parsed.isolation.mcp_aliases, ["opaque-wrapper"]);
    assert!(parsed.isolation.remote["remote"].mcp_inventory_complete);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    assert!(!root.join("db").exists());
    // The removed private_files setting is rejected, never silently ignored.
    for fragment in [
        "[isolation]\nprivate_files=['keys/control.key']",
        "[isolation.remote.remote]\nhost='owner@target'\nmcp_inventory_complete=true\nprivate_files=['~/secret/key']",
    ] {
        let error = loader::parse(&format!("{}{}\n{fragment}\n", basic(root), remote_machine()), &path, &context(root)).unwrap_err();
        assert!(format!("{error:#}").contains("private_files was removed"), "{error:#}");
    }
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
        "[isolation.remote.remote]\nhost='changed-host'\nmcp_inventory_complete=true",
        "[isolation.remote.remote]\nhost='owner@target'",
        "[isolation.remote.remote]\nhost='owner@target'\nsettings_files=['~/../private-secret.toml']",
        "[isolation.remote.remote]\nhost='owner@target'\nmcp_inventory_complete=true\nunknown=true",
        "[isolation.remote.missing]\nhost='owner@target'\nmcp_inventory_complete=true",
        "[isolation.remote.local]\nhost='owner@target'\nmcp_inventory_complete=true",
    ] {
        let error = loader::parse(&format!("{base}\n{fragment}\n"), &root.join("etc/config.toml"), &context(root)).unwrap_err();
        assert!(!format!("{error:#}").contains("private-secret"));
    }
}

#[test]
fn offline_cli_reports_remote_mcp_review_without_probe_or_private_values() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    setup(root);
    let path = root.join("etc/config.toml");
    let base = format!("{}{}", basic(root), remote_machine());
    for configured in [false, true] {
        let inventory = if configured {
            "\n[isolation]\nmcp_aliases=['private-alias']\n[isolation.remote.remote]\nhost='owner@target'\nsettings_files=['~/private/hidden-capability.json']\nmcp_inventory_complete=true\n"
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
            report["isolation"]["remote"][0]["mcp_inventory_complete"],
            configured
        );
        assert_eq!(
            report["isolation"]["remote"][0]["settings_file_count"],
            usize::from(configured)
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

#[test]
fn mcp_source_inventory_resolves_local_files_and_requires_safe_target_paths() {
    let dir = tempfile::tempdir().unwrap();
    // Resolved paths are canonical (macOS temp dirs live under /private/var).
    let root = &dir.path().canonicalize().unwrap();
    setup(root);
    let source = format!("{}{}\n[isolation]\nsettings_files=['sources/extra.toml']\nmcp_inventory_complete=true\n[isolation.remote.remote]\nhost='owner@target'\nsettings_files=['~/sources/managed.json']\nmcp_inventory_complete=true\n", basic(root), remote_machine());
    let config = loader::parse(&source, &root.join("etc/config.toml"), &context(root)).unwrap();
    assert_eq!(
        config.isolation.settings_files,
        vec![root.join("etc/sources/extra.toml")]
    );
    assert_eq!(
        config.isolation.remote["remote"].settings_files,
        vec!["~/sources/managed.json"]
    );
    let summary = config.isolation.summary(&config.machines).to_string();
    assert!(!summary.contains("managed.json"));
    assert!(!summary.contains("extra.toml"));
    assert!(!root.join("etc/sources").exists());
    for fragment in [
        "[isolation]\nsettings_files=['opaque.yaml']",
        "[isolation]\nmcp_inventory_complete='yes'",
        "[isolation.remote.remote]\nhost='owner@target'\nsettings_files=['relative/private.toml']",
        "[isolation.remote.remote]\nhost='owner@target'\nsettings_files=['~/../private.toml']",
    ] {
        assert!(loader::parse(
            &format!("{}{}\n{fragment}", basic(root), remote_machine()),
            &root.join("etc/config.toml"),
            &context(root)
        )
        .is_err());
    }
}

#[test]
fn editor_preserves_comments_validates_and_refuses_external_edits() {
    use config::editor::Prepared;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    let path = dir.path().join("etc/config.toml");
    let source = format!("# owner config\n{}\n[parent] # parent settings\nmodel = 'old' # keep inline\n# workload\n[limits]\nmax_jobs = 3 # capacity\n", basic(dir.path()));
    std::fs::write(&path, &source).unwrap();
    let ctx = context(dir.path());
    let current = config::load(&path, &ctx).unwrap();
    for (section, patch) in [
        ("limits", json!({"max_jobs":0})),
        ("limits", json!({"max_jobs":true})),
        ("limits", json!({"max_jobs":1.5})),
        ("limits", json!({"max_wait_replies":3})),
        ("parent", json!({"model":null})),
        ("parent", json!({"backend":"bogus"})),
        // Implicit machine backends must not change through a parent control.
        ("parent", json!({"backend":"codex"})),
        ("parent", json!({"timeout":1})),
        ("owner", json!({"profile":"new"})),
    ] {
        assert!(
            Prepared::new(&current, section, &patch, &ctx).is_err(),
            "{patch}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    }
    let edit = Prepared::new(
        &current,
        "parent",
        &json!({"model":"new", "triage_model":"fast"}),
        &ctx,
    )
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    edit.commit().unwrap();
    let changed = std::fs::read_to_string(&path).unwrap();
    for comment in [
        "# owner config",
        "# parent settings",
        "# keep inline",
        "# workload",
        "# capacity",
    ] {
        assert!(changed.contains(comment), "lost {comment}: {changed}");
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let current = config::load(&path, &ctx).unwrap();
    assert_eq!(current.parent.model, "new");
    assert_eq!(current.parent.triage_model, "fast");
    let edit = Prepared::new(&current, "limits", &json!({"max_jobs":2}), &ctx).unwrap();
    std::fs::write(&path, format!("{changed}\n# concurrent owner edit\n")).unwrap();
    assert!(edit.commit().is_err());
    assert!(Prepared::new(&current, "limits", &json!({"max_jobs":2}), &ctx).is_err());
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .ends_with("# concurrent owner edit\n"));
    assert!(
        !std::fs::read_dir(path.parent().unwrap()).unwrap().any(|p| p
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".fridica-config-"))
    );
}

#[tokio::test]
async fn configuration_journal_recovers_both_sides_of_rename_and_blocks_conflicts() {
    use config::editor::Prepared;
    use fridica::store::{configuration as journal, Store};
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    let path = dir.path().join("etc/config.toml");
    std::fs::write(&path, basic(dir.path())).unwrap();
    let ctx = context(dir.path());
    let current = config::load(&path, &ctx).unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    // Commit file, lose runtime acknowledgement, then restart with the new file.
    let edit = Prepared::new(&current, "limits", &json!({"max_jobs":2}), &ctx).unwrap();
    journal::replace(&store, edit, 10.).await.unwrap();
    assert!(journal::pending(&store).await.unwrap().is_some());
    assert!(journal::recover_startup(&store, &current, 11.)
        .await
        .is_err());
    let updated = config::load(&path, &ctx).unwrap();
    journal::recover_startup(&store, &updated, 12.)
        .await
        .unwrap();
    journal::recover_startup(&store, &updated, 13.)
        .await
        .unwrap();
    assert!(journal::pending(&store).await.unwrap().is_none());
    // Crash after intent but before rename. Startup records not_applied.
    let edit = Prepared::new(&updated, "limits", &json!({"max_jobs":1}), &ctx).unwrap();
    let intent = journal::Intent {
        path: updated.path.clone(),
        before: updated.fingerprint.clone(),
        after: edit.config.fingerprint,
    };
    let payload = serde_json::to_string(&intent).unwrap();
    let id = store.call(move |c| { c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('configuration_edit',14,?,0)", [payload])?; Ok(c.last_insert_rowid()) }).await.unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{text}\n# unexpected edit\n")).unwrap();
    let external = config::load(&path, &ctx).unwrap();
    assert!(journal::recover_startup(&store, &external, 15.)
        .await
        .is_err());
    assert_eq!(journal::pending(&store).await.unwrap().unwrap().0, id);
    std::fs::write(&path, text).unwrap();
    journal::recover_startup(&store, &updated, 16.)
        .await
        .unwrap();
    let outcomes: Vec<String> = store.call(|c| Ok(c.prepare("SELECT json_extract(payload_json,'$.outcome') FROM replay_events WHERE kind='configuration_result' ORDER BY seq")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)).await.unwrap();
    assert_eq!(outcomes, ["applied", "not_applied"]);
}
#[test]
fn an_egress_deny_list_must_be_private_and_valid() {
    use std::os::unix::fs::PermissionsExt;
    let cases: Vec<Value> = serde_json::from_str(include_str!("corpus/config.json")).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("fe-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = directory.path().canonicalize().unwrap();
    setup(&root);
    let context = context(&root);
    let base = cases.iter().find(|c| c["error"] != true).unwrap()["source"]
        .as_str()
        .unwrap()
        .replace("__ROOT__", root.to_str().unwrap());
    let deny = root.join("etc/deny.txt");
    let path = root.join("etc/config.toml");
    let load = |list: &str, mode: u32| {
        std::fs::write(&deny, list).unwrap();
        std::fs::set_permissions(&deny, std::fs::Permissions::from_mode(mode)).unwrap();
        std::fs::write(
            &path,
            format!("{base}\n[egress]\ndeny_list = \"{}\"\n", deny.display()),
        )
        .unwrap();
        config::load(&path, &context)
    };
    assert_eq!(
        load("Private Name\n", 0o600).unwrap().egress.deny_list,
        Some(deny.clone())
    );
    assert!(load("Private Name\n", 0o644).is_err(), "readable by others");
    assert!(load("(unclosed\n", 0o600).is_err(), "invalid pattern");
    std::fs::write(&path, format!("{base}\n[egress]\nunknown = 1\n")).unwrap();
    assert!(config::load(&path, &context).is_err());
}

#[test]
fn migrate_defaults_to_the_configured_database() {
    use fridica::store::schema;
    let dir = tempfile::tempdir().unwrap();
    let root = &dir.path().canonicalize().unwrap();
    setup(root);
    let config = root.join("etc/config.toml");
    std::fs::write(&config, basic(root)).unwrap();
    let mut c = rusqlite::Connection::open(root.join("db")).unwrap();
    schema::migrate(&mut c).unwrap();
    drop(c);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args(["migrate", "--dry-run", "--config"])
        .arg(&config)
        .env("HOME", root.join("home"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["from"], schema::VERSION);
    assert_eq!(plan["to"], schema::VERSION);
    // An explicit database that is not the configured one is still refused.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args(["migrate", "--dry-run", "--config"])
        .arg(&config)
        .arg("--database")
        .arg(root.join("other.db"))
        .env("HOME", root.join("home"))
        .output()
        .unwrap();
    assert!(!output.status.success());
}
