use fridica::exec::{
    credentials, isolation,
    process::{self, Launch},
    shell, ssh,
};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, time::Duration};

fn fixture() -> (tempfile::TempDir, Launch) {
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("ssh");
    std::fs::write(&executable, "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nshift\nexec /usr/bin/env -i PATH=/usr/bin:/bin GH_TOKEN=target-old TARGET_ONLY_TOKEN=target-keep /bin/sh -c \"$1\"\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let script = shell::join(&["/usr/bin/python3".into(), "-c".into(), "import os,sys,json; print(json.dumps(dict(os.environ)), flush=True); sys.stdout.buffer.write(sys.stdin.buffer.read())".into()]);
    let launch = Launch {
        argv: ssh::command("fixture", &script, dir.path()).unwrap(),
        cwd: None,
        env: BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", dir.path().display()).into(),
            ),
            ("GH_TOKEN".into(), "forwarded-gh-'$()\nsecond-line".into()),
            ("ALPHAXIV_TOKEN".into(), "forwarded-alpha".into()),
            ("INTERLEAF_CONFIG".into(), "/master/config".into()),
            ("INTERLEAF_WORKSPACE".into(), "/master/checkout".into()),
            ("OPENAI_API_KEY".into(), "forwarded-model".into()),
            ("CUSTOM_AUTH".into(), "forwarded-custom".into()),
            (
                "OVERLEAF_NATIVE_SESSION".into(),
                "synthetic-native-envelope".into(),
            ),
            ("GH_CONFIG_DIR".into(), "/master-only/path".into()),
            ("SLACK_USER_TOKEN".into(), "xoxp-private".into()),
            ("DISGUISED_TOKEN".into(), "xoxb-private".into()),
            ("FRIDICA_CONTROL_TOKEN".into(), "private-control".into()),
            ("OWNER_TOKEN".into(), "private-owner".into()),
        ]),
    };
    (dir, launch)
}
#[tokio::test]
async fn ssh_forwards_integration_credentials_without_arguments_or_stdin_corruption() {
    let (_dir, launch) = fixture();
    let mut launch = credentials::forward(
        launch,
        &["OWNER_TOKEN".into()],
        &[
            "CUSTOM_AUTH".into(),
            "OVERLEAF_NATIVE_SESSION".into(),
            "GH_CONFIG_DIR".into(),
            "INTERLEAF_CONFIG".into(),
            "INTERLEAF_WORKSPACE".into(),
        ],
    )
    .unwrap();
    for value in [
        "forwarded-gh",
        "forwarded-alpha",
        "forwarded-model",
        "forwarded-custom",
        "synthetic-native-envelope",
        "private-control",
        "private-owner",
        "xoxp-private",
    ] {
        assert!(!launch.argv.iter().any(|arg| arg.contains(value)));
    }
    assert!(!launch.env.contains_key(std::ffi::OsStr::new("OWNER_TOKEN")));
    isolation::read_only_ssh_probe(&mut launch);
    let input = b"{\"jsonrpc\":\"2.0\",\"id\":1}\nsecond message\n".repeat(5000);
    let result = process::run_once(launch, input.clone(), Duration::from_secs(10), 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        result.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let split = result.stdout.iter().position(|c| *c == b'\n').unwrap();
    let env: BTreeMap<String, String> = serde_json::from_slice(&result.stdout[..split]).unwrap();
    assert_eq!(env["GH_TOKEN"], "forwarded-gh-'$()\nsecond-line");
    assert_eq!(env["ALPHAXIV_TOKEN"], "forwarded-alpha");
    assert_eq!(env["OPENAI_API_KEY"], "forwarded-model");
    assert_eq!(env["CUSTOM_AUTH"], "forwarded-custom");
    assert_eq!(env["OVERLEAF_NATIVE_SESSION"], "synthetic-native-envelope");
    assert_eq!(env["TARGET_ONLY_TOKEN"], "target-keep");
    for name in [
        "SLACK_USER_TOKEN",
        "DISGUISED_TOKEN",
        "FRIDICA_CONTROL_TOKEN",
        "OWNER_TOKEN",
        "GH_CONFIG_DIR",
        "INTERLEAF_CONFIG",
        "INTERLEAF_WORKSPACE",
    ] {
        assert!(!env.contains_key(name));
    }
    assert_eq!(&result.stdout[split + 1..], input);
}
#[test]
fn empty_tokens_keep_the_plain_ssh_launch_and_oversized_values_fail_privately() {
    let (_dir, mut launch) = fixture();
    launch.env = BTreeMap::from([("GH_TOKEN".into(), "".into())]);
    let original = launch.argv.clone();
    assert_eq!(
        credentials::forward(launch, &[], &[]).unwrap().argv,
        original
    );
    let (_dir, mut launch) = fixture();
    launch
        .env
        .insert("GH_TOKEN".into(), "private-large-value".repeat(4000).into());
    let err = credentials::forward(launch, &[], &[])
        .err()
        .unwrap()
        .to_string();
    assert!(!err.contains("private-large-value"));
}
#[tokio::test]
async fn malformed_credential_frame_fails_closed_without_executing_remote_command() {
    let launch = Launch {
        argv: vec![
            "/usr/bin/python3".into(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            credentials::HELPER.into(),
            "receive".into(),
            "printf must-not-run".into(),
        ],
        cwd: None,
        env: BTreeMap::new(),
    };
    let result = process::run_once(launch, vec![0, 0, 0, 4, b'{'], Duration::from_secs(3), 4096)
        .await
        .unwrap();
    assert_eq!(result.returncode, 97);
    assert!(result.stdout.is_empty());
    assert_eq!(result.stderr, b"worker credential transfer failed\n");
}
