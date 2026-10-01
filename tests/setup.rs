use fridica::{
    cli::setup::detected,
    config::{
        self,
        setup::{self, Draft, Identity},
        LoadContext,
    },
    core::delivery::AdapterFuture,
    exec::process::{self, Launch},
    slack::{
        discovery::{self, Api, Channel, Request},
        web::Failure,
    },
};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
struct CliOutput {
    returncode: i32,
    stdout: String,
    stderr: String,
}
struct Fixture {
    dir: tempfile::TempDir,
    path: PathBuf,
    context: LoadContext,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("etc/config.toml");
        let context = LoadContext {
            home: dir.path().join("home"),
            runtime_dir: None,
            uid: users::get_current_uid(),
            protected: vec![],
        };
        std::fs::create_dir(&context.home).unwrap();
        Self { dir, path, context }
    }
    async fn cli(&self, args: &[&str]) -> CliOutput {
        self.cli_env(args, &[]).await
    }
    async fn cli_env(&self, args: &[&str], extra: &[(&str, &str)]) -> CliOutput {
        let mut argv = vec![env!("CARGO_BIN_EXE_fridica").into()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let mut environment =
            BTreeMap::from([("HOME".into(), self.context.home.clone().into_os_string())]);
        environment.extend(extra.iter().map(|(k, v)| ((*k).into(), (*v).into())));
        let output = process::run_once(
            Launch {
                argv,
                cwd: Some(self.dir.path().into()),
                env: environment,
            },
            vec![],
            Duration::from_secs(10),
            16384,
        )
        .await
        .unwrap();
        CliOutput {
            returncode: output.returncode,
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }
    fn read(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap()
    }
}
#[tokio::test]
async fn init_cli_publishes_private_v4_assets_once_without_state_or_workspace_creation() {
    let f = Fixture::new();
    let output = f.cli(&["init", "--config", f.path.to_str().unwrap()]).await;
    assert_eq!(output.returncode, 0, "{}", output.stderr);
    assert_eq!(f.read(), setup::TEMPLATE);
    assert!(f.path.with_file_name("contract.md").is_file());
    assert_eq!(
        std::fs::read_to_string(f.path.with_file_name("manifest.yaml")).unwrap(),
        setup::MANIFEST
    );
    for name in ["config.toml", "contract.md", "manifest.yaml"] {
        assert_eq!(
            std::fs::metadata(f.path.with_file_name(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    assert_eq!(
        std::fs::metadata(f.path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(!f.context.home.join("project").exists());
    assert!(!f.context.home.join(".local").exists());
    assert!(!f.read().contains("max_wait_replies"));
    let before = f.read();
    assert_ne!(
        f.cli(&["init", "--config", f.path.to_str().unwrap()])
            .await
            .returncode,
        0
    );
    assert_eq!(f.read(), before);
    // Full loading still refuses the unconfigured workspace; setup never claims readiness.
    assert!(config::load(&f.path, &f.context).is_err());
    std::fs::create_dir(f.context.home.join("project")).unwrap();
    let config = config::load(&f.path, &f.context).unwrap();
    assert_eq!(
        (
            config.attention.max_echo_replies_per_hour,
            config.attention.max_replies_per_hour,
            config.attention.streak_signal
        ),
        (10, 20, 3)
    );
    assert!(!config.isolation.mcp_inventory_complete);
    let assets = f.cli(&["assets"]).await;
    use sha2::{Digest, Sha256};
    assert_eq!(assets.returncode, 0);
    assert!(assets.stdout.lines().any(|line| line
        == format!(
            "{:x}  template.toml",
            Sha256::digest(setup::TEMPLATE.as_bytes())
        )));
}
#[test]
fn init_preserves_owner_companions_and_refuses_filename_collisions() {
    let f = Fixture::new();
    std::fs::create_dir(f.path.parent().unwrap()).unwrap();
    std::fs::write(f.path.with_file_name("contract.md"), "owner contract").unwrap();
    let target = f.dir.path().join("manifest-owner");
    std::fs::write(&target, "owner manifest").unwrap();
    std::os::unix::fs::symlink(&target, f.path.with_file_name("manifest.yaml")).unwrap();
    setup::init(&f.path).unwrap();
    assert_eq!(
        std::fs::read_to_string(f.path.with_file_name("contract.md")).unwrap(),
        "owner contract"
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "owner manifest");
    assert!(
        std::fs::symlink_metadata(f.path.with_file_name("manifest.yaml"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let bad = f.dir.path().join("new/contract.md");
    assert!(setup::init(&bad).is_err());
    assert!(!bad.parent().unwrap().exists());
}
#[tokio::test]
async fn configure_cli_accepts_incomplete_files_preserves_comments_and_deduplicates_ids() {
    let f = Fixture::new();
    setup::init(&f.path).unwrap();
    let output = f
        .cli(&[
            "configure",
            "--config",
            f.path.to_str().unwrap(),
            "--owner-id",
            "UOWNER",
            "--workspace-id",
            "TTEAM",
            "--channel-id",
            "GPRIVATE",
            "--channel-id",
            "CROOM",
            "--channel-id",
            "GPRIVATE",
        ])
        .await;
    assert_eq!(output.returncode, 0, "{}", output.stderr);
    let source = f.read();
    let doc: toml_edit::DocumentMut = source.parse().unwrap();
    assert_eq!(doc["owner"]["slack_user"].as_str(), Some("UOWNER"));
    assert_eq!(doc["slack"]["workspace"].as_str(), Some("TTEAM"));
    assert_eq!(
        doc["slack"]["channels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>(),
        ["GPRIVATE", "CROOM"]
    );
    assert!(source.contains("# a few sentences about you"));
    assert!(source.contains("# your Slack member ID"));
    assert!(!f.context.home.join(".local").exists());
    for args in [
        vec!["--owner-id", "invalid-private-value"],
        vec!["--workspace-id", "UWRONG"],
        vec!["--channel-id", "DROOM"],
        vec![],
        vec!["--channel-name", "general"],
        vec!["--detect", "--owner-id", "UOWNER"],
    ] {
        let mut command = vec!["configure", "--config", f.path.to_str().unwrap()];
        command.extend(args);
        let output = f.cli(&command).await;
        assert_eq!(output.returncode, 2, "{}", output.stderr);
        assert!(!output.stderr.contains("invalid-private-value"));
        assert_eq!(f.read(), source);
    }
}
#[tokio::test]
async fn setup_cli_defaults_to_home_and_missing_credentials_cannot_modify_the_file() {
    let f = Fixture::new();
    assert_eq!(f.cli(&["init"]).await.returncode, 0);
    let path = f.context.home.join(".config/fridica/config.toml");
    assert!(path.is_file());
    let before = std::fs::read(&path).unwrap();
    let output = f
        .cli(&["configure", "--detect", "--channel-name", "general"])
        .await;
    assert_eq!(output.returncode, 2);
    assert!(output.stderr.contains("SLACK_USER_TOKEN"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    std::fs::write(
        &path,
        std::str::from_utf8(&before)
            .unwrap()
            .replace("SLACK_USER_TOKEN", "CUSTOM_USER_TOKEN"),
    )
    .unwrap();
    let output = f
        .cli_env(
            &["configure", "--detect", "--channel-name", "general"],
            &[("CUSTOM_USER_TOKEN", "xoxb-private-custom")],
        )
        .await;
    assert_eq!(output.returncode, 2);
    assert!(output.stderr.contains("user-token"));
    assert!(!output.stderr.contains("private-custom"));
    assert!(!f.context.home.join(".local").exists());
    assert_eq!(
        f.cli(&[
            "configure",
            "--config",
            "~/.config/fridica/config.toml",
            "--owner-id",
            "WOWNER"
        ])
        .await
        .returncode,
        0
    );
}
#[test]
fn setup_validates_only_identity_but_never_overwrites_a_changed_or_malformed_file() {
    let f = Fixture::new();
    std::fs::create_dir(f.path.parent().unwrap()).unwrap();
    let source = "# unfinished owner config\n[parent]\nbackend='not-yet-valid'\n[slack]\nuser_token_env='CUSTOM_USER_TOKEN'\n";
    std::fs::write(&f.path, source).unwrap();
    let draft = Draft::open(f.path.clone()).unwrap();
    assert_eq!(draft.token_variable().unwrap(), "CUSTOM_USER_TOKEN");
    draft
        .apply(Identity {
            owner: Some("UOWNER".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(f.read().contains("not-yet-valid"));
    let draft = Draft::open(f.path.clone()).unwrap();
    std::fs::write(&f.path, source).unwrap();
    assert!(draft
        .apply(Identity {
            workspace: Some("TTEAM".into()),
            ..Default::default()
        })
        .is_err());
    assert_eq!(f.read(), source);
    for value in ["42", "'bad name'", "'xoxp-private'"] {
        std::fs::write(&f.path, format!("[slack]\nuser_token_env={value}")).unwrap();
        assert!(Draft::open(f.path.clone())
            .unwrap()
            .token_variable()
            .is_err());
    }
    std::fs::write(&f.path, "private-secret-not-toml !!!").unwrap();
    let error = Draft::open(f.path.clone()).err().unwrap().to_string();
    assert!(!error.contains("private-secret"));
}
struct Script {
    replies: Mutex<VecDeque<Result<Value, Failure>>>,
    calls: Mutex<Vec<Request>>,
}
impl Script {
    fn new(replies: Vec<Result<Value, Failure>>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(vec![]),
        }
    }
    fn joined() -> Self {
        Self::new(vec![
            Ok(json!({"user_id":"UOWNER","team_id":"TTEAM"})),
            Ok(json!({"channels":[{"id":"CROOM","name":"general","is_member":true}]})),
            Ok(json!({"channels":[]})),
        ])
    }
}
impl Api for Script {
    fn get(&self, request: Request) -> AdapterFuture<'_, Result<Value, Failure>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected discovery call")
        })
    }
}
#[tokio::test]
async fn discovery_matches_membership_pagination_scope_warnings_and_private_channels() {
    let script = Script::new(vec![
        Ok(json!({"user_id":"WOWNER","team_id":"TTEAM"})),
        Ok(
            json!({"channels":[{"id":"COTHER","name":"other","is_member":false}],"response_metadata":{"next_cursor":"next"}}),
        ),
        Ok(
            json!({"channels":[{"id":"CROOM","name":"general","is_member":true},{"id":"COLD","name":"old","is_member":true,"is_archived":true},{"id":"CBOT","name":"dm","is_member":true,"is_im":true}]}),
        ),
        Err(Failure::Rejected {
            code: "missing_scope".into(),
        }),
    ]);
    let found = discovery::discover(&script).await.unwrap();
    assert_eq!(
        (found.owner.as_str(), found.workspace.as_str()),
        ("WOWNER", "TTEAM")
    );
    assert_eq!(
        found.channels,
        vec![Channel {
            id: "CROOM".into(),
            name: "general".into()
        }]
    );
    assert!(found.warnings[0].contains("groups:read"));
    assert_eq!(
        script.calls.lock().unwrap()[2],
        Request::Channels {
            private: false,
            cursor: "next".into()
        }
    );
    let private = Script::new(vec![
        Ok(json!({"user_id":"UOWNER","team_id":"TTEAM"})),
        Err(Failure::Rejected {
            code: "missing_scope".into(),
        }),
        Ok(json!({"channels":[{"id":"GROOM","name":"private","is_member":true}]})),
    ]);
    let found = discovery::discover(&private).await.unwrap();
    assert_eq!(found.channels[0].id, "GROOM");
    assert!(found.warnings[0].contains("channels:read"));
}
#[tokio::test]
async fn discovery_refuses_bots_cycles_bad_pages_and_unbounded_pagination() {
    for identity in [
        json!({"user_id":"UBOT","team_id":"TTEAM","bot_id":"B1"}),
        json!({"user_id":"UOWNER","team_id":""}),
        json!({"user_id":"U".repeat(65),"team_id":"TTEAM"}),
    ] {
        assert!(discovery::discover(&Script::new(vec![Ok(identity)]))
            .await
            .is_err());
    }
    let identity = json!({"user_id":"UOWNER","team_id":"TTEAM"});
    for rows in [
        vec![json!({"channels":42})],
        vec![json!({"response_metadata":{"next_cursor":42}})],
        vec![json!({"response_metadata":{"next_cursor":"same"}}); 2],
        (0..100)
            .map(|i| json!({"response_metadata":{"next_cursor":i.to_string()}}))
            .collect(),
    ] {
        let replies = std::iter::once(Ok(identity.clone()))
            .chain(rows.into_iter().map(Ok))
            .collect();
        assert!(discovery::discover(&Script::new(replies)).await.is_err());
    }
}
#[test]
fn selection_deduplicates_names_and_numbers_and_rejects_ambiguity_and_cancellation() {
    let channels = vec![
        Channel {
            id: "CROOM".into(),
            name: "general".into(),
        },
        Channel {
            id: "GROOM".into(),
            name: "private".into(),
        },
    ];
    assert_eq!(
        discovery::select_names(
            &channels,
            &["#private".into(), "general".into(), "private".into()]
        )
        .unwrap(),
        ["GROOM", "CROOM"]
    );
    assert_eq!(
        discovery::select_numbers(&channels, "2, 1, 2").unwrap(),
        ["GROOM", "CROOM"]
    );
    for input in [
        "",
        "0",
        "3",
        "one",
        "1,",
        "-1",
        "+1",
        "9999999999999999999999999999",
    ] {
        assert!(discovery::select_numbers(&channels, input).is_err());
    }
    assert!(discovery::select_names(&channels, &["missing".into()]).is_err());
    assert!(discovery::select_names(
        &[channels[0].clone(), channels[0].clone()],
        &["general".into()]
    )
    .is_err());
}
#[tokio::test]
async fn detected_setup_commits_only_after_selection_and_refuses_file_changes_during_discovery() {
    let f = Fixture::new();
    setup::init(&f.path).unwrap();
    let source = f.read();
    let result = detected(
        Draft::open(f.path.clone()).unwrap(),
        &Script::joined(),
        |found| discovery::select_names(&found.channels, &["missing".into()]),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(f.read(), source);
    let result = detected(
        Draft::open(f.path.clone()).unwrap(),
        &Script::joined(),
        |_| {
            std::fs::write(&f.path, format!("{source}\n# owner concurrent edit\n"))?;
            Ok(vec!["CROOM".into()])
        },
    )
    .await;
    assert!(result.is_err());
    assert!(f.read().contains("# owner concurrent edit"));
    detected(
        Draft::open(f.path.clone()).unwrap(),
        &Script::joined(),
        |found| discovery::select_names(&found.channels, &["general".into()]),
    )
    .await
    .unwrap();
    assert!(f.read().contains("UOWNER"));
    assert!(f.read().contains("CROOM"));
    assert!(!f.context.home.join(".local").exists());
}

#[tokio::test]
async fn cancelled_or_failed_detection_and_nonterminal_selection_leave_configuration_unchanged() {
    struct Pending;
    impl Api for Pending {
        fn get(&self, _: Request) -> AdapterFuture<'_, Result<Value, Failure>> {
            Box::pin(std::future::pending())
        }
    }
    let f = Fixture::new();
    setup::init(&f.path).unwrap();
    let original = f.read();
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        detected(Draft::open(f.path.clone()).unwrap(), &Pending, |_| panic!(
            "selection must not run"
        ))
    )
    .await
    .is_err());
    assert_eq!(f.read(), original);
    let api = Script::new(vec![Err(Failure::Connection)]);
    assert!(
        detected(Draft::open(f.path.clone()).unwrap(), &api, |_| panic!(
            "selection must not run"
        ))
        .await
        .is_err()
    );
    let found = discovery::discover(&Script::joined()).await.unwrap();
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        assert!(fridica::cli::setup::select(&found, &[])
            .unwrap_err()
            .to_string()
            .contains("terminal"));
    }
    assert_eq!(
        fridica::cli::setup::select(&found, &["general".into()]).unwrap(),
        ["CROOM"]
    );
    assert_eq!(f.read(), original);
}

#[test]
fn configuration_writers_share_a_private_lock_and_refuse_linked_lock_files() {
    use fs2::FileExt;
    let f = Fixture::new();
    setup::init(&f.path).unwrap();
    let identity = Identity {
        owner: Some("UOWNER".into()),
        ..Default::default()
    };
    Draft::open(f.path.clone())
        .unwrap()
        .apply(identity.clone())
        .unwrap();
    let original = f.read();
    let lock_path = f.path.with_file_name(".config.toml.edit.lock");
    let guard = std::fs::File::open(&lock_path).unwrap();
    guard.try_lock_exclusive().unwrap();
    assert!(Draft::open(f.path.clone())
        .unwrap()
        .apply(Identity {
            workspace: Some("TNEW".into()),
            ..Default::default()
        })
        .is_err());
    assert_eq!(f.read(), original);
    std::fs::create_dir(f.context.home.join("project")).unwrap();
    let current = config::load(&f.path, &f.context).unwrap();
    let live =
        config::editor::Prepared::new(&current, "parent", &json!({"model":"updated"}), &f.context)
            .unwrap();
    assert!(live.commit().is_err());
    assert_eq!(f.read(), original);
    drop(guard);
    live.commit().unwrap();
    let updated = f.read();
    std::fs::remove_file(&lock_path).unwrap();
    let target = f.dir.path().join("untouched");
    std::fs::write(&target, "private owner data").unwrap();
    std::os::unix::fs::symlink(&target, &lock_path).unwrap();
    assert!(Draft::open(f.path.clone())
        .unwrap()
        .apply(identity)
        .is_err());
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "private owner data"
    );
    assert_eq!(f.read(), updated);
}
