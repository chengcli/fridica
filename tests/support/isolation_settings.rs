use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};

fn system_python() -> &'static str {
    [
        "/usr/bin/python3.14",
        "/usr/bin/python3.13",
        "/usr/bin/python3.12",
        "/usr/bin/python3.11",
    ]
    .into_iter()
    .find(|p| std::path::Path::new(p).is_file())
    .expect("Python 3.11+ prerequisite")
}

fn write(path: &std::path::Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn settings_are_sanitized_across_layers_without_changing_owner_files_or_other_servers() {
    let f = Fixture::new();
    let config = f.home.join(".codex/config.toml");
    let profile = f.home.join(".codex/review.config.toml");
    let project = f.workspace.join(".codex/config.toml");
    let claude = f.home.join(".claude.json");
    let project_json = f.workspace.join(".mcp.json");
    let source = r#"
# comment with owner-control-secret must not appear in the snapshot
model = "owner-model"
sample_date = 2026-09-28T12:00:00Z
sample_array = [{name="one", enabled=true}, {name="two", enabled=false}]
[mcp_servers."renamed control"]
command = "/opt/owner/fridica"
args = ["mcp"]
env = { CAPABILITY = "owner-control-secret" }
[mcp_servers.docs]
command = "documentation-server"
args = ["fridica documentation"]
[model_providers.corp]
base_url = "https://provider.invalid/v1"
env_key = "MODEL_AUTH"
"#;
    write(&config, source);
    write(&profile, "[mcp_servers.layered]\ncommand='python3'\nargs=['-m','fridica','mcp']\nenv={KEY='profile-secret'}\n");
    write(
        &project,
        "[mcp_servers.layered]\ncommand='wrapper'\nenv={KEY='project-secret'}\n",
    );
    write(
        &claude,
        r#"{"session":"owner-session","projects":{"/project":{"mcpServers":{"renamed control":{"command":"wrapper","env":{"KEY":"claude-secret"}},"docs":{"command":"documentation-server"}}}}}"#,
    );
    write(
        &project_json,
        r#"{"mcpServers":{"env-alias":{"command":"wrapper","env":{"Fridica_Control_Token":"json-secret"}},"docs":{"command":"documentation-server"}}}"#,
    );
    let originals = [&config, &profile, &project, &claude, &project_json]
        .map(|p| (p, std::fs::read(p).unwrap()));
    symlink(&config, f.workspace.join("config-alias")).unwrap();
    let checks = r#"
import datetime, json, os, pathlib, tomllib
home = pathlib.Path(os.environ['HOME'])
config = home/'.codex/config.toml'
raw = config.read_text()
assert 'owner-control-secret' not in raw
data = tomllib.loads(raw)
assert data['model'] == 'owner-model'
assert data['sample_date'] == datetime.datetime(2026, 9, 28, 12, tzinfo=datetime.timezone.utc)
assert data['sample_array'] == [dict(name='one', enabled=True), dict(name='two', enabled=False)]
assert data['model_providers']['corp']['env_key'] == 'MODEL_AUTH'
assert data['mcp_servers']['docs']['command'] == 'documentation-server'
assert data['mcp_servers']['renamed control'] == dict(command='/bin/false', enabled=False)
assert pathlib.Path('config-alias').read_text() == raw
for p in [home/'.codex/review.config.toml', pathlib.Path('.codex/config.toml')]:
    text = p.read_text()
    assert 'secret' not in text
    assert tomllib.loads(text)['mcp_servers']['layered']['enabled'] is False
    try:
        p.write_text('unsafe')
    except OSError:
        pass
    else:
        raise AssertionError('settings writable')
claude = home/'.claude.json'
data = json.loads(claude.read_text())
assert data['session'] == 'owner-session'
assert set(data['projects']['/project']['mcpServers']) == {'docs'}
claude.write_text('{"session":"worker-private-update"}')
assert set(json.loads(pathlib.Path('.mcp.json').read_text())['mcpServers']) == {'docs'}
for name in os.listdir('/proc/self/fd'):
    if int(name) > 2:
        try:
            os.fstat(int(name))
        except OSError:
            pass
        else:
            raise AssertionError('settings descriptor leaked')
print('sanitized')
"#;
    let mut launch = f.command(checks);
    launch.argv[7] = system_python().into();
    let out = process::run_once(launch, vec![], Duration::from_secs(10), 4096)
        .await
        .unwrap();
    assert_eq!(
        out.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, b"sanitized\n");
    for (path, original) in originals {
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
}

#[tokio::test]
async fn discovered_and_registered_identities_are_disabled_before_backend_start() {
    let f = Fixture::new();
    let config = f.home.join(".codex/config.toml");
    write(
        &config,
        r#"
[mcp_servers."wrapped.🚦"]
command="opaque-wrapper"
env={KEY="wrapped-secret"}
[mcp_servers.http]
url="http://user:http-secret@localhost:8123/fridica?token=http-secret"
[mcp_servers.automatic]
command="fridica"
env={KEY="automatic-secret"}
[mcp_servers.from_factory]
command="opaque-wrapper"
env={KEY="factory-secret"}
"#,
    );
    let fake = f.workspace.join("codex");
    write(&fake, "#!/usr/bin/python3 -I\nimport json, pathlib, os, sys\ntext=(pathlib.Path(os.environ['HOME'])/'.codex/config.toml').read_text()\nassert 'secret' not in text\nprint(json.dumps(sys.argv[1:]))\n");
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let isolation = Isolation::new(&f.config, &[])
        .unwrap()
        .with_mcp_identities(
            &["wrapped.🚦".into()],
            &["http://localhost:8123/fridica".into()],
        )
        .unwrap();
    let launch = isolation
        .launch(
            &f.transport(),
            vec![
                fake.to_str().unwrap().into(),
                "app-server".into(),
                "-c".into(),
                "mcp_servers.from_factory.enabled=false".into(),
            ],
            &f.workspace,
            BTreeMap::new(),
            false,
        )
        .unwrap();
    let out = process::run_once(launch, vec![], Duration::from_secs(10), 4096)
        .await
        .unwrap();
    assert_eq!(
        out.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let args: Vec<String> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(args[0], "app-server");
    for alias in ["wrapped.🚦", "http", "automatic", "from_factory"] {
        let setting = format!("mcp_servers.{}.enabled=false", serde_json::json!(alias));
        assert!(args.contains(&setting), "missing {alias}");
        let parsed: toml_edit::DocumentMut = setting.parse().unwrap();
        assert_eq!(
            parsed["mcp_servers"][alias]["enabled"].as_bool(),
            Some(false)
        );
    }
}

#[tokio::test]
async fn malformed_oversized_linked_settings_and_uninventoried_homes_refuse_before_startup() {
    for kind in [
        "toml",
        "json",
        "duplicate_json",
        "large",
        "symlink",
        "hardlink",
        "custom_home",
    ] {
        let f = Fixture::new();
        let config = f.home.join(".codex/config.toml");
        write(&config, "");
        match kind {
            "toml" => write(&config, "private-secret = ["),
            "json" => write(&f.home.join(".claude.json"), "private-secret"),
            "duplicate_json" => write(
                &f.home.join(".claude.json"),
                r#"{"mcpServers":{},"mcpServers":{"private-secret":{}}}"#,
            ),
            "large" => write(&config, &"#".repeat(1024 * 1024 + 1)),
            "symlink" => {
                std::fs::remove_file(&config).unwrap();
                symlink(&f.config.path, &config).unwrap();
            }
            "hardlink" => std::fs::hard_link(&config, f.workspace.join("settings-alias")).unwrap(),
            _ => {}
        }
        let mut launch = f.command("print('backend started')");
        if kind == "custom_home" {
            launch
                .env
                .insert("CODEX_HOME".into(), f.workspace.clone().into_os_string());
        }
        let out = process::run_once(launch, vec![], Duration::from_secs(5), 4096)
            .await
            .unwrap();
        assert_eq!(
            out.returncode,
            97,
            "{kind}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.stdout.is_empty(), "{kind}");
        assert_eq!(
            out.stderr, b"fridica worker isolation: setup refused\n",
            "{kind}"
        );
    }
}

#[test]
fn registered_mcp_identities_reject_credential_urls_and_control_characters() {
    let f = Fixture::new();
    for url in [
        "relative",
        "https://user:secret@host/",
        "https://host/?key=secret",
        "https://host/#secret",
    ] {
        assert!(Isolation::new(&f.config, &[])
            .unwrap()
            .with_mcp_identities(&[], &[url.into()])
            .is_err());
    }
    assert!(Isolation::new(&f.config, &[])
        .unwrap()
        .with_mcp_identities(&["bad\nalias".into()], &[])
        .is_err());
}
