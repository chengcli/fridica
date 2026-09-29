use fridica::{
    config::{loader, LoadContext},
    core::worker::ArtifactRef,
    exec::{shell, ssh::SshTransport},
    workers::{
        artifacts::SystemJobIo,
        protocol::{JobIo, WorkerSpec},
    },
};
use serde_json::json;
use std::{
    collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, path::PathBuf,
    time::Duration,
};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    spec: WorkerSpec,
    environment: BTreeMap<OsString, OsString>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for path in ["remote/work", "bin", "private"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        std::fs::set_permissions(root.join("private"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let source="[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[machines.remote]\ntransport='ssh'\nhost='owner@fixture'\n[machines.remote.workspaces]\nproject='~/work'\n[state]\npath='private/db'\ncontrol_socket='private/control.sock'";
        let config = loader::parse(
            source,
            &root.join("config.toml"),
            &LoadContext {
                home: root.clone(),
                runtime_dir: None,
                uid: users::get_current_uid(),
                protected: vec![],
            },
        )
        .unwrap();
        let machine = &config.machines.machines[0];
        let spec=serde_json::from_value(json!({"worker_id":"w", "machine":machine,"workspace":machine.workspaces[0],"backend":"codex","instructions":"","model":"","reasoning_effort":"","job_timeout":30,"idle_timeout":30,"excluded_env":["OWNER_SECRET"],"slot":1})).unwrap();
        let environment = BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", root.join("bin").display()).into(),
            ),
            ("HOME".into(), root.clone().into_os_string()),
            ("OWNER_SECRET".into(), "private-token".into()),
        ]);
        let f = Self {
            _dir: dir,
            root,
            spec,
            environment,
        };
        f.ssh(&format!("#!/bin/sh\ntest -z \"${{OWNER_SECRET+x}}\" || exit 91\nwhile [ \"$1\" != -- ]; do shift; done\nshift\ntest \"$1\" = owner@fixture || exit 255\nshift\nexport HOME={}\nexec /bin/sh -c \"$1\"\n",shell::quote(f.root.join("remote").to_str().unwrap())));
        f
    }
    fn ssh(&self, source: &str) {
        let file = self.root.join("bin/ssh");
        std::fs::write(&file, source).unwrap();
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn transport(&self) -> SshTransport {
        SshTransport {
            machine: self.spec.machine.clone(),
            excluded_env: self.spec.excluded_env.clone(),
            control_directory: self.root.join("private"),
        }
    }
    fn io(&self) -> SystemJobIo {
        SystemJobIo {
            home: self.root.clone(),
            environment: self.environment.clone(),
            ssh_control_directory: self.root.join("private"),
            read_timeout: Duration::from_secs(5),
        }
    }
    async fn read(&self, path: &str, limit: usize) -> anyhow::Result<Vec<u8>> {
        self.transport()
            .read_file(
                path.into(),
                vec!["~/work".into()],
                limit,
                self.environment.clone(),
                Duration::from_secs(5),
            )
            .await
    }
}
#[tokio::test]
async fn target_home_binary_frames_and_stable_links_preserve_artifact_content() {
    let f = Fixture::new();
    let filename = "quote' ; $(touch NOT_EXECUTED).md";
    let bytes = b"# target report\n\0binary payload";
    std::fs::write(f.root.join("remote/work").join(filename), bytes).unwrap();
    std::os::unix::fs::symlink(filename, f.root.join("remote/work/link.md")).unwrap();
    assert_eq!(
        f.read(&format!("~/work/{filename}"), bytes.len())
            .await
            .unwrap(),
        bytes
    );
    assert_eq!(f.read("~/work/link.md", bytes.len()).await.unwrap(), bytes);
    assert!(!f.root.join("remote/work/NOT_EXECUTED").exists());
    assert!(f.read("~/work/link.md", bytes.len() - 1).await.is_err());
    std::fs::write(f.root.join("remote/work/empty.md"), []).unwrap();
    assert!(f.read("~/work/empty.md", 0).await.unwrap().is_empty());
}
#[tokio::test]
async fn escaping_links_traversal_hardlinks_and_special_files_refuse_before_transfer() {
    let f = Fixture::new();
    let private = f.root.join("remote/private.md");
    std::fs::write(&private, "private-content-must-never-transfer").unwrap();
    std::os::unix::fs::symlink(&private, f.root.join("remote/work/escape.md")).unwrap();
    std::fs::hard_link(&private, f.root.join("remote/work/hard.md")).unwrap();
    // rustix has no mknodat on Apple targets; the utility is portable.
    assert!(std::process::Command::new("mkfifo")
        .args(["-m", "400"])
        .arg(f.root.join("remote/work/pipe.md"))
        .status()
        .unwrap()
        .success());
    for path in [
        "~/private.md",
        "~/work/../private.md",
        "~/work/escape.md",
        "~/work/hard.md",
        "~/work/pipe.md",
        "~/work",
        "~/work/missing.md",
    ] {
        let error = f.read(path, 1024).await.unwrap_err().to_string();
        assert!(!error.contains("private-content"));
    }
}
#[tokio::test]
async fn job_io_validates_content_and_preserves_individual_rejections() {
    let f = Fixture::new();
    std::fs::write(f.root.join("remote/work/report.md"), "# remote").unwrap();
    std::fs::write(
        f.root.join("remote/work/invalid.pdf"),
        "private invalid content",
    )
    .unwrap();
    let references = [
        ("~/work/report.md", "md"),
        ("~/work/invalid.pdf", "pdf"),
        ("~/outside.md", "md"),
    ]
    .into_iter()
    .map(|(path, kind)| ArtifactRef {
        path: path.into(),
        kind: kind.into(),
        caption: "report".into(),
    })
    .collect();
    let result = f.io().collect(f.spec.clone(), references).await.unwrap();
    assert_eq!(result[0].data.as_deref(), Some(b"# remote".as_slice()));
    assert_eq!(result[1].error, "artifact_invalid_pdf");
    assert_eq!(result[2].error, "artifact_read_failed");
    assert!(result[1].data.is_none() && result[2].data.is_none());
}
#[tokio::test]
async fn truncated_banners_failed_exits_and_timeout_never_accept_partial_artifacts() {
    let f = Fixture::new();
    for source in [
        "#!/bin/sh\nexit 255\n",
        "#!/bin/sh\nprintf 'login banner\\n'\n",
        "#!/bin/sh\nprintf 'fridica-artifact-v1\\000'\n",
        "#!/usr/bin/python3\nimport sys\nsys.stdout.buffer.write(b'fridica-artifact-v1\\0'+(99).to_bytes(8,'big')+b'partial')\n",
        "#!/usr/bin/python3\nimport sys\nsys.stdout.buffer.write(b'fridica-artifact-v1\\0'+(2).to_bytes(8,'big')+b'ok')\nsys.exit(255)\n",
    ] {f.ssh(source);assert!(f.read("~/work/report.md",1024).await.is_err());}
    let marker = f.root.join("pid");
    f.ssh(&format!(
        "#!/bin/sh\necho $$ > {}\ntrap '' TERM\nexec /bin/sleep 60\n",
        shell::quote(marker.to_str().unwrap())
    ));
    let result = f
        .transport()
        .read_file(
            "~/work/report.md".into(),
            vec!["~/work".into()],
            1024,
            f.environment.clone(),
            Duration::from_millis(200),
        )
        .await;
    assert!(result.is_err());
    let pid = rustix::process::Pid::from_raw(
        std::fs::read_to_string(marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
}
#[tokio::test]
async fn cancelling_the_read_reaps_its_owned_ssh_process() {
    let f = Fixture::new();
    let marker = f.root.join("pid");
    f.ssh(&format!(
        "#!/bin/sh\necho $$ > {}\ntrap '' TERM\nexec /bin/sleep 60\n",
        shell::quote(marker.to_str().unwrap())
    ));
    let transport = f.transport();
    let env = f.environment.clone();
    let read = tokio::spawn(async move {
        transport
            .read_file(
                "~/work/report.md".into(),
                vec!["~/work".into()],
                1024,
                env,
                Duration::from_secs(30),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = rustix::process::Pid::from_raw(
        std::fs::read_to_string(marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    )
    .unwrap();
    read.abort();
    let _ = read.await;
    tokio::time::timeout(Duration::from_secs(4), async {
        while rustix::process::test_kill_process(pid).is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
