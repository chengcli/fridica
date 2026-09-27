use fridica::exec::{
    process::{self, Launch, Process},
    sandbox, shell, ssh,
};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
fn launch(script: &str, cwd: &Path) -> Launch {
    Launch {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: Some(cwd.into()),
        env: std::env::vars_os().collect(),
    }
}
#[tokio::test]
async fn subprocess_drains_both_pipes_while_writing_and_preserves_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let script = "head -c 262144 /dev/zero >&2; cat; printf done; exit 7";
    let c = process::run_once(
        launch(script, dir.path()),
        vec![b'x'; 262144],
        Duration::from_secs(5),
        1024 * 1024,
    )
    .await
    .unwrap();
    assert_eq!(c.returncode, 7);
    assert_eq!(c.stderr.len(), 262144);
    assert_eq!(c.stdout.len(), 262148);
    assert!(c.stdout.ends_with(b"done"));
}
#[tokio::test]
async fn eof_wait_reports_delayed_exit_and_never_invents_success() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Process::start(&launch(
        "exec 1>&-; sleep .15; printf failure >&2; exit 23",
        dir.path(),
    ))
    .unwrap();
    let mut stdout = child.stdout().unwrap();
    assert_eq!(stdout.read(&mut [0; 1]).await.unwrap(), 0);
    assert!(child
        .eof_status(Duration::from_millis(10))
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        process::returncode(
            child
                .eof_status(Duration::from_secs(2))
                .await
                .unwrap()
                .unwrap()
        ),
        23
    );
    let mut stderr = vec![];
    child
        .stderr()
        .unwrap()
        .read_to_end(&mut stderr)
        .await
        .unwrap();
    assert_eq!(stderr, b"failure");
    child.terminate(Duration::from_millis(20)).await.unwrap();
}
#[tokio::test]
async fn oversized_stdout_and_stderr_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    for script in ["head -c 5000 /dev/zero", "head -c 5000 /dev/zero >&2"] {
        let error = process::run_once(
            launch(script, dir.path()),
            vec![],
            Duration::from_secs(3),
            1000,
        )
        .await
        .err()
        .unwrap();
        assert!(error.to_string().contains("size limit"), "{error}");
    }
}
async fn file(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(s) = std::fs::read_to_string(path) {
                if !s.trim().is_empty() {
                    return s;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
#[cfg(target_os = "linux")]
fn running(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()))
        .ok()
        .is_some_and(|s| {
            s.rsplit_once(") ")
                .is_some_and(|(_, s)| !s.starts_with('Z'))
        })
}
#[cfg(not(target_os = "linux"))]
fn running(pid: &str) -> bool {
    let pid = rustix::process::Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
    rustix::process::test_kill_process(pid).is_ok()
}
async fn gone(pid: &str) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while running(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn timeout_cancellation_and_exited_leaders_do_not_leave_descendants() {
    let dir = tempfile::tempdir().unwrap();
    for kind in ["timeout", "cancel", "leader-exit"] {
        let marker = dir.path().join(kind);
        let script = format!(
            "sh -c 'trap \"\" TERM; echo $$ > {}; sleep 60' & {}",
            shell::quote(marker.to_str().unwrap()),
            if kind == "leader-exit" {
                format!(
                    "while [ ! -s {} ]; do sleep .01; done; exit 0",
                    shell::quote(marker.to_str().unwrap())
                )
            } else {
                "wait".into()
            }
        );
        let task = tokio::spawn(process::run_once(
            launch(&script, dir.path()),
            vec![],
            Duration::from_millis(250),
            1024,
        ));
        let pid = file(&marker).await;
        if kind == "cancel" {
            task.abort();
            let _ = task.await;
        } else if kind == "leader-exit" {
            assert_eq!(task.await.unwrap().unwrap().returncode, 0);
        } else {
            assert!(task.await.unwrap().is_err());
        }
        gone(&pid).await;
    }
}
#[test]
fn environment_scrubbing_and_diagnostics_do_not_restore_or_disclose_tokens() {
    let inherited = [
        ("SLACK_USER_TOKEN", "secret"),
        ("OTHER", "xoxb-secret"),
        ("CUSTOM", "custom-secret"),
        ("KEEP", "yes"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()));
    let extra = BTreeMap::from([
        ("SLACK_USER_TOKEN".into(), "restored".into()),
        ("EXTRA".into(), "xapp-hidden".into()),
        ("OMP_NUM_THREADS".into(), "3".into()),
    ]);
    let env = process::scrubbed_environment(inherited, &["CUSTOM".into()], &extra);
    assert_eq!(env.len(), 2);
    assert_eq!(env.get(std::ffi::OsStr::new("KEEP")).unwrap(), "yes");
    let line = process::diagnostic(
        b"\ncredential=custom-secret\n token=xoxp-123-abc rest",
        500,
        &["custom-secret".into()],
    );
    assert_eq!(line, "credential=[redacted] token=[redacted] rest");
    assert_eq!(process::diagnostic("a雪雪雪".as_bytes(), 2, &[]), "雪雪");
}
#[tokio::test]
async fn remote_template_quotes_untrusted_words_and_home_paths() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join("my repo")).unwrap();
    let hostile = "it's $HOME `touch INJECTED` \\ \"q\";$(touch INJECTED)";
    for program in ["sh", "bash"] {
        let script = ssh::remote_script(
            &[
                "sh".into(),
                "-c".into(),
                "printf '%s|%s|%s' \"$1\" \"$PWD\" \"$OMP_NUM_THREADS\"".into(),
                "-".into(),
                hostile.into(),
            ],
            "~/my repo",
            &BTreeMap::from([("OMP_NUM_THREADS".into(), "4".into())]),
            Some(Duration::from_secs(5)),
            false,
        )
        .unwrap();
        let mut spec = launch(&script, dir.path());
        spec.argv[0] = program.into();
        spec.env.insert("HOME".into(), home.as_os_str().into());
        let out = process::run_once(spec, vec![], Duration::from_secs(10), 4096)
            .await
            .unwrap();
        assert_eq!(
            out.returncode,
            0,
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            out.text(),
            format!("{hostile}|{}/my repo|4", home.display())
        );
        assert!(!dir.path().join("INJECTED").exists());
    }
    assert!(ssh::remote_script(
        &["true".into()],
        "/tmp",
        &BTreeMap::from([("BAD; touch BAD".into(), "x".into())]),
        None,
        false
    )
    .is_err());
}
#[tokio::test]
async fn remote_watchdog_preserves_status_and_cleans_private_fifo() {
    let dir = tempfile::tempdir().unwrap();
    for program in ["sh", "bash"] {
        let script = ssh::remote_script(
            &[
                "sh".into(),
                "-c".into(),
                "read line; printf 'done:%s' \"$line\"; exit 7".into(),
            ],
            dir.path().to_str().unwrap(),
            &BTreeMap::new(),
            None,
            false,
        )
        .unwrap();
        let mut spec = launch(&script, dir.path());
        spec.argv[0] = program.into();
        spec.env
            .insert("TMPDIR".into(), dir.path().as_os_str().into());
        let out = process::run_once(spec, b"x\n".to_vec(), Duration::from_secs(10), 4096)
            .await
            .unwrap();
        assert_eq!((out.returncode, out.text()), (7, "done:x".into()));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn remote_channel_eof_and_wrapper_signal_kill_agents_and_tools() {
    let dir = tempfile::tempdir().unwrap();
    for stop in ["eof", "signal"] {
        let agent = dir.path().join(format!("agent-{stop}"));
        let tool = dir.path().join(format!("tool-{stop}"));
        let script=ssh::remote_script(&["sh".into(),"-c".into(),format!("echo $$ > {}; (sleep 60 & echo $! > {}; wait) & while read line; do :; done; sleep 60",shell::quote(agent.to_str().unwrap()),shell::quote(tool.to_str().unwrap()))],dir.path().to_str().unwrap(),&BTreeMap::new(),None,false).unwrap();
        let mut spec = launch(&script, dir.path());
        spec.env
            .insert("TMPDIR".into(), dir.path().as_os_str().into());
        let mut child = Process::start(&spec).unwrap();
        let mut stdin = child.stdin().unwrap();
        stdin.write_all(b"ping\n").await.unwrap();
        let agent_pid = file(&agent).await;
        let tool_pid = file(&tool).await;
        if stop == "eof" {
            drop(stdin);
        } else {
            rustix::process::kill_process(
                rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
                rustix::process::Signal::TERM,
            )
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(8), child.wait())
            .await
            .unwrap()
            .unwrap();
        gone(&agent_pid).await;
        gone(&tool_pid).await;
        assert!(!std::fs::read_dir(dir.path()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("fridica.")));
        child.terminate(Duration::from_millis(50)).await.unwrap();
    }
}
#[test]
fn ssh_control_directory_rejects_symlinks_permissions_and_long_paths() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let dir = tempfile::tempdir().unwrap();
    let uid = users::get_current_uid();
    let control = ssh::control_directory(Some(dir.path()), uid).unwrap();
    let argv = ssh::command("owner@dart9", "exec sh -c true", &control).unwrap();
    assert_eq!(&argv[..2], &["ssh", "-T"]);
    assert!(!argv.contains(&"-A".into()));
    assert_eq!(
        &argv[argv.len() - 3..],
        &["--", "owner@dart9", "exec sh -c true"]
    );
    std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ssh::control_directory(Some(dir.path()), uid).is_err());
    std::fs::remove_dir(&control).unwrap();
    symlink(dir.path(), &control).unwrap();
    assert!(ssh::control_directory(Some(dir.path()), uid).is_err());
    assert!(ssh::command("-Fbad", "true", dir.path()).is_err());
}
#[test]
fn confinement_preserves_backend_settings_and_quotes_home_references() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
    std::fs::write(dir.path().join(".codex/config.toml"), "model='mine'").unwrap();
    sandbox::prepare_local(dir.path()).unwrap();
    sandbox::prepare_local(dir.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap(),
        "model='mine'"
    );
    let words = sandbox::confinement(&["/work".into(), "~/my repo".into()], None).unwrap();
    assert_eq!(&words[..2], &["bwrap", "--die-with-parent"]);
    assert!(words.contains(&"--ro-bind-try".into()));
    assert!(sandbox::shell_words(&words).contains("\"$HOME/\"'my repo'"));
}

#[tokio::test]
async fn local_launch_applies_resource_environment_and_creates_slot_directories() {
    use fridica::{config::registry::Machine, exec::local::LocalTransport};
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let m:Machine=serde_json::from_value(serde_json::json!({"name":"local","transport":"local","workspaces":[],"backends":["codex"],"default_backend":"codex","policy":{},"host":"","tags":[],"resources":{"cpus":3,"gpus":[1]},"max_workers":1,"max_jobs":1,"slurm":null,"description":""})).unwrap();
    let local = LocalTransport {
        machine: m,
        home: home.clone(),
        excluded_env: vec!["PRIVATE".into()],
    };
    let cwd = home.join("work/worker1");
    let spec = local
        .launch(
            vec![
                "sh".into(),
                "-c".into(),
                "printf '%s|%s|%s' \"$PWD\" \"$CUDA_VISIBLE_DEVICES\" \"$OMP_NUM_THREADS\"".into(),
            ],
            &cwd,
            [("PRIVATE".into(), "token".into())],
            &BTreeMap::new(),
            None,
            true,
        )
        .unwrap();
    assert!(!spec.env.contains_key(std::ffi::OsStr::new("PRIVATE")));
    let done = process::run_once(spec, vec![], Duration::from_secs(5), 1024)
        .await
        .unwrap();
    assert_eq!(
        done.text(),
        format!("{}|1|3", std::fs::canonicalize(cwd).unwrap().display())
    );
    let spec = local
        .launch(
            vec!["true".into()],
            Path::new("~/work/worker1"),
            [],
            &BTreeMap::new(),
            Some(&["~/work/worker1".into()]),
            false,
        )
        .unwrap();
    assert_eq!(spec.argv[0], "bwrap");
    assert!(home.join(".claude/settings.json").exists());
}
#[test]
fn local_artifact_reads_confine_symlinks_types_and_size() {
    use fridica::exec::local::read_file;
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let roots = vec![root.clone()];
    std::fs::write(root.join("notes.md"), b"# hi").unwrap();
    std::fs::write(dir.path().join("secret"), b"no").unwrap();
    symlink("notes.md", root.join("inside.md")).unwrap();
    symlink("../secret", root.join("escape.md")).unwrap();
    assert_eq!(
        read_file(&root.join("inside.md"), &roots, 100).unwrap(),
        b"# hi"
    );
    for path in [
        root.join("escape.md"),
        root.join("missing.md"),
        dir.path().join("secret"),
        root.clone(),
        root.join("../secret"),
    ] {
        assert!(read_file(&path, &roots, 100).is_err());
    }
    assert!(read_file(&root.join("notes.md"), &roots, 3).is_err());
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        root.join("fifo"),
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    assert!(read_file(&root.join("fifo"), &roots, 100).is_err());
}
#[test]
fn swapping_an_artifact_path_never_reads_outside_the_root() {
    use fridica::exec::local::read_file;
    use std::{
        os::unix::fs::symlink,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(dir.path().join("secret"), b"SECRET").unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let writer_root = root.clone();
    let writer = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            std::fs::write(writer_root.join("good"), b"ok").unwrap();
            std::fs::rename(writer_root.join("good"), writer_root.join("current")).unwrap();
            symlink("../secret", writer_root.join("bad")).unwrap();
            std::fs::rename(writer_root.join("bad"), writer_root.join("current")).unwrap();
        }
    });
    let mut leaked = false;
    for _ in 0..1000 {
        if let Ok(bytes) = read_file(&root.join("current"), std::slice::from_ref(&root), 100) {
            leaked |= bytes != b"ok";
        }
    }
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
    assert!(!leaked);
}

#[tokio::test]
async fn frozen_transport_and_artifact_projections_match_with_one_explicit_exception() {
    use fridica::core::worker::ArtifactRef;
    use fridica::workers::artifacts::{validate_content, validate_reference};
    use serde_json::{json, Value};
    let corpus: Value = serde_json::from_str(include_str!("corpus/transport.json")).unwrap();
    for case in corpus["quotes"].as_array().unwrap() {
        assert_eq!(
            shell::quote(case["input"].as_str().unwrap()),
            case["expected"]
        );
    }
    for case in corpus["sandboxes"].as_array().unwrap() {
        let roots: Vec<String> = serde_json::from_value(case["roots"].clone()).unwrap();
        let argv = sandbox::confinement(&roots, case["home"].as_str()).unwrap();
        assert_eq!(json!(argv), case["argv"]);
        assert_eq!(sandbox::shell_words(&argv), case["shell"]);
    }
    for case in corpus["environments"].as_array().unwrap() {
        let inherited: BTreeMap<String, String> =
            serde_json::from_value(case["inherited"].clone()).unwrap();
        let extra = serde_json::from_value(case["extra"].clone()).unwrap();
        let excluded = serde_json::from_value::<Vec<String>>(case["excluded"].clone()).unwrap();
        let actual: BTreeMap<_, _> = process::scrubbed_environment(
            inherited.into_iter().map(|(k, v)| (k.into(), v.into())),
            &excluded,
            &extra,
        )
        .into_iter()
        .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
        .collect();
        let mut expected = case["expected"].clone();
        if let Some(removed) = case["exception"]["removed_keys"].as_array() {
            for key in removed {
                expected
                    .as_object_mut()
                    .unwrap()
                    .remove(key.as_str().unwrap())
                    .unwrap();
            }
        }
        assert_eq!(json!(actual), expected);
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("my repo")).unwrap();
    let canonical_home = std::fs::canonicalize(dir.path()).unwrap();
    for case in corpus["remote"].as_array().unwrap() {
        let command = serde_json::from_value::<Vec<String>>(case["command"].clone()).unwrap();
        let env = serde_json::from_value(case["env"].clone()).unwrap();
        let script = ssh::remote_script(
            &command,
            case["cwd"].as_str().unwrap(),
            &env,
            Some(Duration::from_secs(case["timeout"].as_u64().unwrap())),
            false,
        )
        .unwrap();
        let mut spec = launch(&script, dir.path());
        spec.env
            .insert("HOME".into(), canonical_home.as_os_str().into());
        let out = process::run_once(spec, vec![], Duration::from_secs(10), 4096)
            .await
            .unwrap();
        assert_eq!(
            json!({"status":out.returncode,"stdout":out.text().replace(canonical_home.to_str().unwrap(),"/HOME")}),
            case["expected"]
        );
    }
    for case in corpus["contents"].as_array().unwrap() {
        let reference = ArtifactRef {
            path: "/work/result".into(),
            kind: case["kind"].as_str().unwrap().into(),
            caption: String::new(),
        };
        let data = serde_json::from_value::<Vec<u8>>(case["data"].clone()).unwrap();
        assert_eq!(
            validate_content(&reference, &data).is_ok(),
            case["accepted"].as_bool().unwrap()
        );
    }
    for case in corpus["references"].as_array().unwrap() {
        let reference: ArtifactRef = serde_json::from_value(case["reference"].clone()).unwrap();
        assert_eq!(
            validate_reference(&reference).is_ok(),
            case["accepted"].as_bool().unwrap()
        );
    }
}

#[tokio::test]
async fn ssh_launch_round_trip_uses_existing_options_and_resource_environment() {
    use fridica::{
        config::registry::Machine,
        exec::ssh::{LaunchOptions, SshTransport},
    };
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("ssh");
    std::fs::write(&fake,"#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\ntest \"$1\" = dart9 || exit 255\nshift\nexec sh -c \"$1\"\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let machine:Machine=serde_json::from_value(serde_json::json!({"name":"remote","transport":"ssh","workspaces":[],"backends":["codex"],"default_backend":"codex","policy":{},"host":"dart9","tags":[],"resources":{"cpus":2},"max_workers":1,"max_jobs":1,"slurm":null,"description":""})).unwrap();
    let transport = SshTransport {
        machine,
        excluded_env: vec!["PRIVATE".into()],
        control_directory: ssh::control_directory(Some(dir.path()), users::get_current_uid())
            .unwrap(),
    };
    let cwd = dir.path().join("work/worker1");
    for create in [false, true] {
        let mut spec = transport
            .launch(
                vec![
                    "sh".into(),
                    "-c".into(),
                    "cat; printf '|%s|%s' \"$PWD\" \"$OMP_NUM_THREADS\"".into(),
                ],
                cwd.to_str().unwrap(),
                std::env::vars_os(),
                &BTreeMap::from([("PRIVATE".into(), "hidden".into())]),
                LaunchOptions {
                    timeout: Some(Duration::from_secs(5)),
                    create,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(!spec.argv.last().unwrap().contains("hidden"));
        assert!(spec.cwd.is_none());
        spec.argv[0] = fake.to_str().unwrap().into();
        let done = process::run_once(spec, b"input".to_vec(), Duration::from_secs(10), 1024)
            .await
            .unwrap();
        if create {
            assert_eq!(done.returncode, 0);
            assert_eq!(done.text(), format!("input|{}|2", cwd.display()));
        } else {
            assert_eq!(done.returncode, 98);
        }
    }
    assert!(ssh::failure("dart9", 255, b"token xoxp-secret", &[]).contains("[redacted]"));
    assert!(ssh::failure("dart9", 98, b"", &[]).contains("workspace directory does not exist"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn escaped_descendant_pipes_cannot_prevent_bounded_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("escaped");
    let command = format!(
        "setsid sh -c 'echo $$ > {}; sleep 60' & while [ ! -s {} ]; do sleep .01; done; exit 0",
        shell::quote(marker.to_str().unwrap()),
        shell::quote(marker.to_str().unwrap())
    );
    let started = std::time::Instant::now();
    let task = tokio::spawn(process::run_once(
        launch(&command, dir.path()),
        vec![],
        Duration::from_millis(500),
        1024,
    ));
    let escaped = file(&marker).await;
    let outcome = task.await.unwrap();
    let elapsed = started.elapsed();
    // setsid deliberately escapes the owned group. The test owns this helper
    // and explicitly removes it; transport teardown must release its open pipes.
    let pid = rustix::process::Pid::from_raw(escaped.trim().parse().unwrap()).unwrap();
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    gone(&escaped).await;
    assert!(outcome.is_err());
    assert!(elapsed < Duration::from_secs(3));
}
