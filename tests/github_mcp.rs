use fridica::{
    config::{loader, Config, LoadContext},
    exec::process::{self, Launch},
    github::mcp::{self, Target},
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, time::Duration};

const SERVER: &str = r#"#!/usr/bin/python3
import json, os, sys
assert sys.argv[1] == 'stdio'
assert os.environ['GITHUB_PERSONAL_ACCESS_TOKEN'] == 'test-private-token'
assert os.environ['GITHUB_HOST'] == 'https://github.com'
assert 'GITHUB_TOOLSETS' not in os.environ
assert 'GH_TOKEN' not in os.environ
assert 'GITHUB_TOKEN' not in os.environ
assert not any('SLACK' in k or k.startswith('FRIDICA_') for k in os.environ)
assert os.getcwd() == '/'
tools = sys.argv[sys.argv.index('--tools') + 1].split(',')
if '--read-only' in sys.argv:
    assert 'push_files' not in tools and 'pull_request_review_write' not in tools
else:
    assert 'push_files' in tools and 'pull_request_review_write' in tools
assert 'merge_pull_request' not in tools
mode = os.environ.get('TEST_MODE', '')
if mode == 'hang':
    import pathlib, time
    pathlib.Path(os.environ['TEST_PID']).write_text(str(os.getpid()))
    while True: time.sleep(1)
if mode == 'extra': tools.append('merge_pull_request')
if mode == 'missing': tools.remove('pull_request_read')
for line in sys.stdin:
    req = json.loads(line)
    method = req['method']
    if 'id' not in req: continue
    if method == 'initialize':
        result = {'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':name,'inputSchema':{'type':'object'}} for name in tools]}
    elif method == 'tools/call':
        name = req['params']['name']
        if os.environ.get('PROBE_ONLY') == '1': assert name == 'get_me'
        assert name in tools
        if mode == 'denied':
            result = {'isError':True,'content':[{'type':'text','text':'permission denied'}]}
        elif name == 'get_me':
            result = {'content':[{'type':'text','text':'{"login":"reviewer"}'}]}
        else:
            result = {'content':[{'type':'text','text':json.dumps({'tool':name,'arguments':req['params']['arguments'],'url':'https://github.com/o/r/pull/1#review-1'})}]}
    else:
        raise AssertionError(method)
    print(json.dumps({'jsonrpc':'2.0','id':req['id'],'result':result}), flush=True)
"#;
struct Fixture {
    dir: tempfile::TempDir,
    config: Config,
    target: Target,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for path in ["home", "project", "private", "bin"] {
            std::fs::create_dir(dir.path().join(path)).unwrap();
        }
        let exe = dir.path().join("bin/github-mcp-server");
        std::fs::write(&exe, SERVER).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = loader::parse(
            "[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[machines.local.policy]\ngpu_confine=false\nmode='write'\nnetwork=['api.github.com']\n[machines.local.workspaces]\nproject='project'\n[state]\npath='private/db'\ncontrol_socket='private/control.sock'\n",
            &dir.path().join("config.toml"),
            &LoadContext { home: dir.path().join("home"), runtime_dir: None, uid: users::get_current_uid(), protected: vec![] },
        ).unwrap();
        Self {
            target: Target {
                command: exe.to_string_lossy().into_owned(),
                token_env: "WORKER_GITHUB_TOKEN".into(),
            },
            dir,
            config,
        }
    }
    fn environment(&self) -> BTreeMap<std::ffi::OsString, std::ffi::OsString> {
        [
            ("PATH", "/usr/bin:/bin".to_owned()),
            (
                "HOME",
                self.dir.path().join("home").to_string_lossy().into_owned(),
            ),
            ("WORKER_GITHUB_TOKEN", "test-private-token".into()),
            ("GITHUB_TOOLSETS", "all".into()),
            ("GITHUB_HOST", "https://wrong.invalid".into()),
            ("SLACK_TOKEN", "private-slack".into()),
            ("FRIDICA_TOKEN", "private-control".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect()
    }
    fn launch(&self, readonly: bool, probe: bool, mode: &str) -> Launch {
        let server = mcp::server(&self.target, readonly);
        let mut argv = vec![server.command];
        argv.extend(server.args);
        if probe {
            argv.push("--probe".into());
        }
        assert!(!argv.join(" ").contains("test-private-token"));
        let mut env = self.environment();
        env.insert("TEST_MODE".into(), mode.into());
        if probe {
            env.insert("PROBE_ONLY".into(), "1".into());
        }
        Launch {
            argv,
            env,
            cwd: Some(self.dir.path().join("project")),
        }
    }
}

#[test]
fn configuration_defaults_to_bundled_and_placement_policy_filters_tools() {
    let mut f = Fixture::new();
    let policy = f.config.machines.machines[0].workspaces[0].policy.clone();
    let bundled = mcp::servers(&f.config, "local", &policy);
    assert_eq!(bundled.len(), 1);
    assert_eq!(bundled[0].args[0], "github-mcp");
    assert_eq!(bundled[0].command, "fridica");
    f.config.github.mcp.enabled = false;
    assert!(mcp::servers(&f.config, "local", &policy).is_empty());
    f.config.github.mcp.enabled = true;
    f.config
        .github
        .mcp
        .machines
        .insert("local".into(), f.target.clone());
    fridica::config::github_mcp::validate(&f.config).unwrap();
    assert_eq!(mcp::servers(&f.config, "local", &policy).len(), 1);
    for domain in ["*", "*.github.com", "API.GITHUB.COM"] {
        let mut permitted = policy.clone();
        permitted.network = vec![domain.into()];
        assert_eq!(mcp::servers(&f.config, "local", &permitted).len(), 1);
    }
    let mut offline = policy.clone();
    offline.network.clear();
    assert!(mcp::servers(&f.config, "local", &offline).is_empty());
    let mut fetch = policy.clone();
    fetch.fetch_repos = vec!["o/r".into()];
    assert!(mcp::servers(&f.config, "local", &fetch).is_empty());
    let mut ro = policy;
    ro.mode = "read-only".into();
    let server = mcp::servers(&f.config, "local", &ro).remove(0);
    assert!(server.auto_approve);
    assert!(server.args.last().unwrap().contains("\"readonly\":true"));
    for name in [
        "SLACK_APP_TOKEN",
        "FRIDICA_GITHUB_TOKEN",
        "NOT-AN-ENV",
        "1TOKEN",
    ] {
        f.config
            .github
            .mcp
            .machines
            .get_mut("local")
            .unwrap()
            .token_env = name.into();
        assert!(fridica::config::github_mcp::validate(&f.config).is_err());
    }
    f.config.github.mcp.machines.clear();
    f.config
        .github
        .mcp
        .machines
        .insert("unknown".into(), f.target);
    assert!(fridica::config::github_mcp::validate(&f.config).is_err());
}

#[tokio::test]
async fn probe_checks_protocol_identity_and_exact_read_or_write_tools_without_writes() {
    let f = Fixture::new();
    for readonly in [false, true] {
        assert!(mcp::probe_launch(f.launch(readonly, true, ""), Duration::from_secs(20)).await);
    }
    for mode in ["extra", "missing", "denied"] {
        let result = process::run_with_open_stdin(
            f.launch(false, true, mode),
            Duration::from_secs(20),
            4096,
        )
        .await
        .unwrap();
        assert_ne!(result.returncode, 0);
        assert!(result.stdout.is_empty());
        assert_eq!(
            result.stderr,
            b"fridica GitHub MCP: setup or probe failed\n"
        );
    }
}

#[tokio::test]
async fn stdio_forwards_review_comment_and_commit_results_and_permission_errors() {
    let f = Fixture::new();
    let mut input = String::new();
    for (id, name) in [
        "pull_request_read",
        "add_issue_comment",
        "pull_request_review_write",
        "push_files",
    ]
    .iter()
    .enumerate()
    {
        input += &format!(
            "{}\n",
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":{"owner":"o","repo":"r","commit_id":"a".repeat(40)}}})
        );
    }
    let result = process::run_once(
        f.launch(false, false, ""),
        input.as_bytes().to_vec(),
        Duration::from_secs(20),
        8192,
    )
    .await
    .unwrap();
    assert_eq!(result.returncode, 0);
    let results: Vec<Value> = result
        .text()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(results.len(), 4);
    let review: Value =
        serde_json::from_str(results[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(review["arguments"]["commit_id"], "a".repeat(40));
    assert!(review["url"]
        .as_str()
        .unwrap()
        .starts_with("https://github.com/"));
    let denied = process::run_once(
        f.launch(false, false, "denied"),
        input.into_bytes(),
        Duration::from_secs(20),
        8192,
    )
    .await
    .unwrap();
    assert!(denied
        .text()
        .lines()
        .all(|l| serde_json::from_str::<Value>(l).unwrap()["result"]["isError"] == true));
}

#[tokio::test]
async fn target_local_launch_uses_existing_credentials_and_reports_unavailable_server() {
    let mut f = Fixture::new();
    f.config.github.mcp.enabled = true;
    f.config
        .github
        .mcp
        .machines
        .insert("local".into(), f.target.clone());
    let machine = &f.config.machines.machines[0];
    assert!(mcp::probe(
        &f.config,
        &f.dir.path().join("home"),
        f.environment(),
        machine,
        &machine.workspaces[0],
        Duration::from_secs(20)
    )
    .await
    .unwrap());
    let mut target = f.target.clone();
    target.command = "/missing/github-server".into();
    let mut launch = f.launch(false, true, "");
    launch.argv = mcp::probe_command(&target, false);
    assert!(!mcp::probe_launch(launch, Duration::from_secs(20)).await);
}

#[test]
fn summaries_keep_confirmed_github_links_without_a_second_signoff() {
    let text = format!(
        "VERDICT o/r#1 {} approve\nApproved https://github.com/o/r/pull/1#review-1",
        "a".repeat(40)
    );
    let summary = fridica::threads::verdict::github_summary(&text);
    assert!(summary.contains("https://github.com/o/r/pull/1#review-1"));
    assert!(!summary.contains("SIGN-OFF") && !summary.contains("VERDICT"));
    let failed = "Could not submit the review: permission denied.";
    assert_eq!(fridica::threads::verdict::github_summary(failed), failed);
}

#[test]
fn worker_and_parent_instructions_describe_available_tools_without_claiming_publication() {
    use fridica::workers::{
        backend::Instructions,
        instructions::{OwnerInstructions, GITHUB_TOOLS},
    };
    let mut f = Fixture::new();
    let worker = serde_json::from_value(json!({"id":"w","session_id":"s","machine":"local","workspace":"project","backend":"codex"})).unwrap();
    assert!(OwnerInstructions
        .build(&f.config, &worker)
        .unwrap()
        .contains(GITHUB_TOOLS));
    f.config.github.mcp.enabled = false;
    assert!(!OwnerInstructions
        .build(&f.config, &worker)
        .unwrap()
        .contains(GITHUB_TOOLS));
    f.config.github.mcp.enabled = true;
    let prompt = OwnerInstructions.build(&f.config, &worker).unwrap();
    assert!(prompt.contains(GITHUB_TOOLS));
    assert!(prompt.contains("inspect GitHub before retrying"));
    assert!(prompt.contains("review new changes if it moved"));
    let request = serde_json::from_value(json!({"inbox_id":1,"call":"decide","session":{"id":"s","channel":"CROOM","work":{"workers":[]}},"trigger":{},"history":[],"obligations":[],"previous":null,"errors":[]})).unwrap();
    let (prompt, _, _) = fridica::parent::prompts::build(&f.config, &request).unwrap();
    let data: Value = serde_json::from_str(prompt.rsplit_once("\n\nData:\n").unwrap().1).unwrap();
    assert_eq!(
        data["github_mcp_workspaces"][0],
        json!({"machine":"local","workspace":"project","read_only":false})
    );
    assert!(!prompt.contains("test-private-token"));
}

#[tokio::test]
async fn ssh_probe_forwards_worker_credentials_over_stdin() {
    let mut f = Fixture::new();
    f.config.github.mcp.enabled = true;
    f.config
        .github
        .mcp
        .machines
        .insert("local".into(), f.target.clone());
    f.config.machines.machines[0].transport = "ssh".into();
    f.config.machines.machines[0].host = "owner@fixture".into();
    let ssh = f.dir.path().join("bin/ssh");
    std::fs::write(&ssh, "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\ntest \"$1\" = owner@fixture || exit 255\nshift\nexport WORKER_GITHUB_TOKEN=stale-target-token\nexec /bin/sh -c \"$1\"\n").unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut env = f.environment();
    env.insert(
        "PATH".into(),
        format!("{}:/usr/bin:/bin", f.dir.path().join("bin").display()).into(),
    );
    env.insert("WORKER_GITHUB_TOKEN".into(), "test-private-token".into());
    let machine = &f.config.machines.machines[0];
    assert!(mcp::probe(
        &f.config,
        &f.dir.path().join("home"),
        env,
        machine,
        &machine.workspaces[0],
        Duration::from_secs(20)
    )
    .await
    .unwrap());
}

#[tokio::test]
async fn missing_token_uses_target_gh_auth_and_never_records_its_output() {
    let f = Fixture::new();
    let gh = f.dir.path().join("bin/gh");
    std::fs::write(&gh, "#!/bin/sh\ntest \"$*\" = 'auth token --hostname github.com' || exit 1\nprintf test-private-token\n").unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut launch = f.launch(false, true, "");
    launch
        .env
        .remove(std::ffi::OsStr::new("WORKER_GITHUB_TOKEN"));
    launch.env.insert(
        "PATH".into(),
        format!("{}:/usr/bin:/bin", f.dir.path().join("bin").display()).into(),
    );
    let out = process::run_with_open_stdin(launch, Duration::from_secs(20), 4096)
        .await
        .unwrap();
    assert_eq!(out.returncode, 0);
    assert_eq!(out.stdout, b"fridica-github-mcp:ready\n");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("test-private-token"));
}

#[tokio::test]
async fn cancelling_a_probe_reaps_the_mcp_server() {
    let f = Fixture::new();
    let pid_path = f.dir.path().join("pid");
    let mut launch = f.launch(false, true, "hang");
    launch
        .env
        .insert("TEST_PID".into(), pid_path.clone().into_os_string());
    let task = tokio::spawn(mcp::probe_launch(launch, Duration::from_secs(30)));
    let pid = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(pid) = std::fs::read_to_string(&pid_path) {
                if !pid.is_empty() {
                    break pid;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let alive = tokio::process::Command::new("/bin/kill")
                .args(["-0", &pid])
                .output()
                .await
                .unwrap()
                .status
                .success();
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
