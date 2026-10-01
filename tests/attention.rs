use fridica::{
    attention::{self, Answer, Capacity, Message},
    config::Attention,
    core::{Authority, ThreadControl},
    store::Store,
};
use rusqlite::params;

fn message(i: usize) -> Message {
    Message {
        files: vec![],
        event_id: format!("e{i}"),
        workspace: "TTEAM".into(),
        channel: "CROOM".into(),
        ts: format!("100.{i:06}"),
        thread_ts: Some("100.000001".into()),
        sender: "UALICE".into(),
        text: "<@UOWNER> help".into(),
        source: "socket".into(),
        meta: None,
        attachments: vec![],
    }
}
async fn intake(s: &Store, i: usize) -> i64 {
    attention::intake(
        s,
        message(i),
        "UOWNER".into(),
        100. + i as f64,
        900.,
        format!("o{i}"),
    )
    .await
    .unwrap()
    .unwrap()
}
fn session() -> String {
    "TTEAM:CROOM:100.000001".into()
}

#[tokio::test]
async fn duplicate_intake_and_pauses_do_not_lose_mentions() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("state.sqlite3")).await.unwrap();
    intake(&s, 1).await;
    s.call(|c| {
        c.execute("UPDATE threads SET control='paused'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(attention::intake(
        &s,
        message(1),
        "UOWNER".into(),
        2.,
        900.,
        "duplicate".into()
    )
    .await
    .unwrap()
    .is_none());
    intake(&s, 2).await;
    assert_eq!(
        s.call(
            |c| Ok(c.query_row("SELECT count(*) FROM obligations", [], |r| r
                .get::<_, i64>(0))?)
        )
        .await
        .unwrap(),
        2
    );
    assert_eq!(
        s.call(|c| Ok(c.query_row("SELECT control FROM threads", [], |r| r.get::<_, String>(0))?))
            .await
            .unwrap(),
        "paused"
    );
}

#[tokio::test]
async fn answer_closes_only_after_confirmed_delivery_and_due_events_are_deduplicated() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("state.sqlite3")).await.unwrap();
    let inbox = intake(&s, 1).await;
    attention::reserve(
        &s,
        session(),
        inbox,
        "human".into(),
        101.,
        "r1".into(),
        Attention::default(),
    )
    .await
    .unwrap();
    assert_eq!(attention::sweep(&s, 2000.).await.unwrap(), 1);
    assert_eq!(attention::sweep(&s, 2000.).await.unwrap(), 0);
    let post = attention::queue_answer(
        &s,
        Answer {
            key: "a1".into(),
            session: session(),
            channel: "CROOM".into(),
            thread_ts: "100.000001".into(),
            text: "Done".into(),
            obligations: vec!["o1".into()],
            inbox,
        },
        102.,
    )
    .await
    .unwrap();
    assert_eq!(attention::sweep(&s, 2000.).await.unwrap(), 0);
    assert!(attention::delivered(&s, post, "1.0".into(), 103.)
        .await
        .is_err());
    assert_eq!(
        s.call(
            |c| Ok(c.query_row("SELECT state FROM obligations", [], |r| r
                .get::<_, String>(0))?)
        )
        .await
        .unwrap(),
        "awaiting_delivery"
    );
    s.call(move |c| {
        c.execute("UPDATE outbox SET state='sending' WHERE id=?", [post])?;
        Ok(())
    })
    .await
    .unwrap();
    attention::delivered(&s, post, "1.0".into(), 103.)
        .await
        .unwrap();
    assert_eq!(
        s.call(
            |c| Ok(c.query_row("SELECT state FROM obligations", [], |r| r
                .get::<_, String>(0))?)
        )
        .await
        .unwrap(),
        "answered"
    );
}

#[tokio::test]
async fn peer_ceiling_persists_and_deferred_items_do_not_block_controls() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("state.sqlite3")).await.unwrap();
    let limit = Attention::default().max_echo_replies_per_hour;
    for i in 1..=limit + 1 {
        let inbox = intake(&s, i).await;
        let outcome = attention::reserve(
            &s,
            session(),
            inbox,
            "peer".into(),
            200.,
            format!("r{i}"),
            Attention::default(),
        )
        .await
        .unwrap();
        if i <= limit {
            assert_eq!(outcome, Capacity::Reserved);
        } else {
            assert_eq!(outcome, Capacity::Deferred(3800.));
        }
        if i <= limit {
            s.call(move |c| {
                c.execute("UPDATE thread_inbox SET state='done' WHERE id=?", [inbox])?;
                Ok(())
            })
            .await
            .unwrap();
        }
    }
    let control = s
        .call(|c| {
            c.execute(
                "INSERT INTO thread_inbox(session_id,kind,created) VALUES(?,'control',201)",
                [session()],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
        .unwrap();
    assert_eq!(
        attention::claim_due(&s, session(), 201.).await.unwrap(),
        Some((control, "control".into()))
    );
    // Only one inbox item may be processing in a thread. Complete the control
    // before advancing time to the deferred message's next eligible turn.
    s.call(move |c| {
        c.execute("UPDATE thread_inbox SET state='done' WHERE id=?", [control])?;
        Ok(())
    })
    .await
    .unwrap();
    let rows = s
        .call(|c| {
            Ok(
                c.query_row("SELECT count(*) FROM reply_reservations", [], |r| {
                    r.get::<_, i64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(rows, limit as i64);
    assert!(attention::signal_streak(&s, session(), 3, 3, 202.)
        .await
        .unwrap());
    assert_eq!(
        s.call(|c| Ok(c.query_row("SELECT control FROM threads", [], |r| r.get::<_, String>(0))?))
            .await
            .unwrap(),
        "active"
    );
    let deferred = attention::claim_due(&s, session(), 3800.)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(deferred.1, "message");
}

#[test]
fn owner_pause_cannot_be_resumed_by_overseer_or_lead() {
    let paused = ThreadControl::Paused {
        by: Authority::Owner,
        reason: "away".into(),
        since: 1.,
    };
    assert!(paused.can_resume(&Authority::Owner));
    assert!(!paused.can_resume(&Authority::Overseer));
    assert!(!paused.can_resume(&Authority::Lead {
        campaign: "one".into()
    }));
    assert!(!paused.can_resume(&Authority::System));
}

#[tokio::test]
async fn bad_obligation_rolls_back_post_and_reservation_link() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("state.sqlite3")).await.unwrap();
    let inbox = intake(&s, 1).await;
    attention::reserve(
        &s,
        session(),
        inbox,
        "human".into(),
        101.,
        "r1".into(),
        Attention::default(),
    )
    .await
    .unwrap();
    let answer = Answer {
        key: "bad".into(),
        session: session(),
        channel: "CROOM".into(),
        thread_ts: "100.000001".into(),
        text: "Done".into(),
        obligations: vec!["missing".into()],
        inbox,
    };
    assert!(attention::queue_answer(&s, answer, 102.).await.is_err());
    let count = s
        .call(move |c| {
            Ok(c.query_row(
                "SELECT count(*) FROM outbox WHERE idem_key=?",
                params!["bad"],
                |r| r.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn hourly_ceiling_survives_reopen_counts_delivery_time_and_exempts_owner_controls() {
    use fridica::{core::delivery::DeliveryOutcome, store::outbox};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let s = Store::open(path.clone()).await.unwrap();
    for n in 1..=20 {
        let inbox = intake(&s, n).await;
        attention::reserve(
            &s,
            session(),
            inbox,
            if n <= 6 { "peer" } else { "human" }.into(),
            200.,
            format!("r{n}"),
            Attention::default(),
        )
        .await
        .unwrap();
        attention::queue_answer(
            &s,
            Answer {
                key: format!("a{n}"),
                session: session(),
                channel: "CROOM".into(),
                thread_ts: "100.000001".into(),
                text: format!("Answer {n}"),
                obligations: vec![format!("o{n}")],
                inbox,
            },
            201.,
        )
        .await
        .unwrap();
        let claim = outbox::claim(&s, 500.).await.unwrap().unwrap();
        outbox::complete(
            &s,
            claim,
            DeliveryOutcome::Sent {
                reference: format!("500.{n:06}"),
            },
            "UOWNER".into(),
            500.,
        )
        .await
        .unwrap();
    }
    let blocked = intake(&s, 21).await;
    assert_eq!(
        attention::reserve(
            &s,
            session(),
            blocked,
            "human".into(),
            600.,
            "blocked".into(),
            Attention::default()
        )
        .await
        .unwrap(),
        Capacity::Deferred(4100.)
    );
    drop(s);
    // The dedicated database thread drains before releasing its lock.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let s = loop {
        match Store::open(path.clone()).await {
            Ok(s) => break s,
            Err(error) if tokio::time::Instant::now() < deadline => {
                assert!(
                    error.to_string().contains("holds the state database"),
                    "{error:#}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            Err(error) => panic!("database did not close: {error:#}"),
        }
    };
    assert_eq!(
        attention::reserve(
            &s,
            session(),
            blocked,
            "human".into(),
            4000.,
            "blocked-again".into(),
            Attention::default()
        )
        .await
        .unwrap(),
        Capacity::Deferred(4100.)
    );
    let owner = intake(&s, 22).await;
    assert_eq!(
        attention::reserve(
            &s,
            session(),
            owner,
            "owner".into(),
            4000.,
            "owner".into(),
            Attention::default()
        )
        .await
        .unwrap(),
        Capacity::Reserved
    );
    assert_eq!(
        attention::reserve(
            &s,
            session(),
            blocked,
            "human".into(),
            4100.,
            "due".into(),
            Attention::default()
        )
        .await
        .unwrap(),
        Capacity::Reserved
    );
}
