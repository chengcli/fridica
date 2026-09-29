use super::*;

#[tokio::test]
async fn group_eperm_falls_back_to_the_owned_leader_and_reaps_its_real_exit() {
    let mut child = Process::start(&Launch {
        argv: vec!["/bin/sleep".into(), "600".into()],
        cwd: None,
        env: BTreeMap::new(),
    })
    .unwrap();
    let pid = child.group;
    signal_group(
        pid,
        Signal::TERM,
        |actual, signal| {
            assert_eq!(actual, pid);
            assert_eq!(signal, Signal::TERM);
            Err(rustix::io::Errno::PERM)
        },
        kill_process,
    )
    .unwrap();
    let status = child
        .eof_status(Duration::from_secs(1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(returncode(status), -15);
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    child.terminate(Duration::from_millis(100)).await.unwrap();
}

#[test]
fn signal_failures_do_not_hide_permission_errors_or_target_an_absent_group_leader() {
    let pid = Pid::from_raw(1).unwrap(); // Injected calls only: PID 1 is never signalled.
    for error in [
        None,
        Some(rustix::io::Errno::SRCH),
        Some(rustix::io::Errno::INVAL),
    ] {
        let result = signal_group(
            pid,
            Signal::KILL,
            |_, _| error.map_or(Ok(()), Err),
            |_, _| panic!("unexpected leader signal"),
        );
        assert_eq!(result.is_ok(), error != Some(rustix::io::Errno::INVAL));
    }
    for error in [rustix::io::Errno::SRCH, rustix::io::Errno::PERM] {
        let result = signal_group(
            pid,
            Signal::KILL,
            |_, _| Err(rustix::io::Errno::PERM),
            |_, _| Err(error),
        );
        assert_eq!(result.is_ok(), error == rustix::io::Errno::SRCH);
    }
}
