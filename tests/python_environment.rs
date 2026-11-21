use fridica::exec::{
    process::{self, Launch},
    python, ssh,
};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, time::Duration};

#[tokio::test]
async fn local_and_ssh_launches_activate_real_venvs_and_preserve_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let env_name = "python env ' quoted";
    let environment = dir.path().join(env_name);
    assert!(std::process::Command::new("python3")
        .args(["-m", "venv", "--without-pip"])
        .arg(&environment)
        .status()
        .unwrap()
        .success());
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let fake_ssh = bin.join("ssh");
    std::fs::write(
        &fake_ssh,
        "#!/bin/sh\nfor arg do remote_script=$arg; done\nexec /bin/sh -c \"$remote_script\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    for transport in ["local", "ssh"] {
        let args = vec!["python".into(), "-c".into(), "import os,sys; assert os.path.realpath(sys.prefix) == os.path.realpath(os.environ['VIRTUAL_ENV']), (sys.prefix, os.environ['VIRTUAL_ENV']); assert 'PYTHONHOME' not in os.environ; print(os.environ['VIRTUAL_ENV']); sys.stdout.write(sys.stdin.read())".into()];
        let mut launch = Launch {
            argv: if transport == "ssh" {
                ssh::command("fixture", &fridica::exec::shell::join(&args), dir.path()).unwrap()
            } else {
                args
            },
            cwd: Some(dir.path().to_owned()),
            env: BTreeMap::from([
                ("HOME".into(), dir.path().as_os_str().to_owned()),
                (
                    "PATH".into(),
                    format!("{}:/usr/bin:/bin", bin.display()).into(),
                ),
                ("PYTHONHOME".into(), "/invalid-python-home".into()),
            ]),
        };
        launch = python::activate(
            launch,
            transport,
            &format!("~/{env_name}"),
            Some(dir.path()),
        )
        .unwrap();
        let result = process::run_once(
            launch,
            b"worker stdin stays intact\n".to_vec(),
            Duration::from_secs(10),
            4096,
        )
        .await
        .unwrap();
        assert_eq!(
            result.returncode,
            0,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            result.text(),
            format!("{}\nworker stdin stays intact\n", environment.display())
        );
    }
}

#[tokio::test]
async fn missing_environment_refuses_to_run_the_command() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("should-not-run");
    let launch = Launch {
        argv: vec![
            "/usr/bin/touch".into(),
            marker.to_string_lossy().into_owned(),
        ],
        cwd: None,
        env: BTreeMap::new(),
    };
    let launch = python::activate(
        launch,
        "local",
        dir.path().join("missing").to_str().unwrap(),
        None,
    )
    .unwrap();
    let result = process::run_once(launch, vec![], Duration::from_secs(5), 4096)
        .await
        .unwrap();
    assert_eq!(result.returncode, 97);
    assert!(!marker.exists());
    assert_eq!(
        String::from_utf8_lossy(&result.stderr),
        "fridica: configured Python environment unavailable\n"
    );
}

#[test]
fn machine_python_environment_is_host_configuration_and_validates_paths() {
    use fridica::config::{loader, LoadContext};
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("private")).unwrap();
    let context = LoadContext {
        home: dir.path().to_owned(),
        runtime_dir: None,
        uid: users::get_current_uid(),
        protected: vec![],
    };
    let source = "[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[machines.dart10]\nhost='dart10'\nPYTHON_ENV\n[machines.dart10.workspaces]\nproject='/workspace'\n[state]\npath='private/db'\ncontrol_socket='private/control.sock'\n";
    for value in ["~/pyenv", "/opt/python env"] {
        let config = loader::parse(
            &source.replace(
                "PYTHON_ENV",
                &format!("python_env={}", serde_json::json!(value)),
            ),
            &dir.path().join("config.toml"),
            &context,
        )
        .unwrap();
        assert_eq!(config.python_envs["dart10"], value);
        assert!(config
            .machines
            .get("dart10")
            .unwrap()
            .payload(0)
            .get("python_env")
            .is_none());
    }
    for value in ["relative/venv", "~/../escape", "/", ""] {
        assert!(loader::parse(
            &source.replace(
                "PYTHON_ENV",
                &format!("python_env={}", serde_json::json!(value))
            ),
            &dir.path().join("config.toml"),
            &context
        )
        .is_err());
    }
    let config = loader::parse(
        &source.replace("PYTHON_ENV", ""),
        &dir.path().join("config.toml"),
        &context,
    )
    .unwrap();
    assert!(config.python_envs.is_empty());
    assert!(serde_json::to_value(config)
        .unwrap()
        .get("python_envs")
        .is_none());
}

#[tokio::test]
#[ignore = "requires FRIDICA_TEST_SSH_HOST and FRIDICA_TEST_PYTHON_ENV"]
async fn configured_environment_reaches_a_real_ssh_worker() {
    use fridica::{
        config::{loader, LoadContext},
        workers::{
            launcher::{Launcher, SystemLauncher},
            protocol::WorkerSpec,
        },
    };
    let host = std::env::var("FRIDICA_TEST_SSH_HOST").unwrap();
    let environment = std::env::var("FRIDICA_TEST_PYTHON_ENV").unwrap();
    let dir = tempfile::Builder::new()
        .prefix("fridica-env-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let context = LoadContext::current().unwrap();
    let source = format!("[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[machines.remote]\nhost={}\npython_env={}\n[machines.remote.policy]\ngpu_confine=false\n[machines.remote.workspaces]\nproject='/tmp'\n[state]\npath='db'\ncontrol_socket='control.sock'\n",serde_json::json!(host),serde_json::json!(environment));
    let config = loader::parse(&source, &dir.path().join("config.toml"), &context).unwrap();
    let machine = config.machines.get("remote").unwrap();
    let spec: WorkerSpec =
        serde_json::from_value(serde_json::json!({"worker_id":"probe", "machine":machine,
        "workspace":machine.workspaces[0], "backend":"codex", "instructions":"", "model":"",
        "reasoning_effort":"", "job_timeout":30, "idle_timeout":30, "excluded_env":[], "slot":0,
        "python_env":config.python_envs["remote"]}))
        .unwrap();
    let inherited = ["PATH", "HOME", "SSH_AUTH_SOCK"]
        .into_iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (k.into(), v)))
        .collect();
    let launcher =
        SystemLauncher::from_config(&config, context.home, inherited, dir.path().to_owned())
            .unwrap();
    let launch = launcher.launch(&spec, vec!["python".into(), "-c".into(),
        "import os,sys; expected=os.path.realpath(os.path.expanduser(sys.argv[1])); assert os.path.realpath(sys.prefix)==expected; assert os.path.realpath(os.environ['VIRTUAL_ENV'])==expected; print('configured-python:ready')".into(), environment]).unwrap();
    let result = process::run_once(launch, vec![], Duration::from_secs(30), 4096)
        .await
        .unwrap();
    assert_eq!(
        result.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.text(), "configured-python:ready\n");
}
