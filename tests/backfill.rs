use fridica::{
    attention::backfill::{self, Request},
    config::{loader, Config, LoadContext},
    core::Authority,
    store::Store,
};
use rusqlite::params;
use serde_json::{json, Value};
struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    store: Store,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("project")).unwrap();
        let config=loader::parse("[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[machines.local.workspaces]\nproject='project'\n[state]\npath='private/db'\ncontrol_socket='private/control.sock'",&root.join("config.toml"),&LoadContext {home:root.into(),runtime_dir:None,uid:users::get_current_uid(),protected:vec![]}).unwrap();
        let store = Store::open(config.state.path.clone()).await.unwrap();
        store.call(|c| {
            for (index,(id,channel,control,reset)) in [("active","CROOM","active",0.),("paused","CROOM","paused",0.),("cleaned","CROOM","active",150.),("archived","CROOM","archived",0.),("foreign","COTHER","active",0.)].into_iter().enumerate() {
                c.execute("INSERT INTO threads(id,workspace,channel,root_ts,created,updated,control,reset_at,control_json) VALUES(?,'TTEAM',?,?,1,1,?,?,?)",params![id,channel,id,control,reset,json!({"kind":control,"by":"owner"}).to_string()])?;
                c.execute("INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,source,received_at) VALUES(?,'TTEAM',?,?,?,'UALICE','<@UOWNER> old ask','history',100.1)",params![id,channel,format!("100.{}",index+1),id])?;
            }
            c.execute("INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,source,received_at) VALUES('own','TTEAM','CROOM','101.1','active','UOWNER','<@UOWNER> own','history',101.1)",[])?;
            c.execute("INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,source,received_at) VALUES('unmentioned','TTEAM','CROOM','102.1','active','UALICE','Other text','history',102.1)",[])?;
            Ok(())
        }).await.unwrap();
        Self {
            _dir: dir,
            config,
            store,
        }
    }
    async fn run(&self, apply: bool) -> Value {
        backfill::run(
            &self.store,
            &self.config,
            Request {
                since: 90.,
                until: 110.,
                apply,
                client_id: if apply { "backfill-1234" } else { "" }.into(),
            },
            Authority::Owner,
            200.,
        )
        .await
        .unwrap()
    }
}
#[tokio::test]
async fn preview_and_idempotent_apply_preserve_pauses_scope_and_unknown_answer_status() {
    let f = Fixture::new().await;
    let preview = f.run(false).await;
    assert_eq!(preview["count"], 2);
    assert_eq!(
        f.store
            .call(
                |c| Ok(c.query_row("SELECT count(*) FROM obligations", [], |r| r
                    .get::<_, i64>(0))?)
            )
            .await
            .unwrap(),
        0
    );
    let result = f.run(true).await;
    assert_eq!(result["count"], 2);
    assert_eq!(result["answer_status"], "unknown");
    assert_eq!(f.run(true).await, result);
    assert_eq!(f.run(false).await["count"], 0);
    f.store
        .call(|c| {
            assert_eq!(
                c.query_row(
                    "SELECT count(*) FROM obligations WHERE state='deferred' AND due=1100",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                2
            );
            assert_eq!(
                c.query_row("SELECT control FROM threads WHERE id='paused'", [], |r| {
                    r.get::<_, String>(0)
                })?,
                "paused"
            );
            assert_eq!(
                c.query_row("SELECT count(*) FROM thread_inbox", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                c.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                c.query_row(
                    "SELECT count(*) FROM audit WHERE action='obligations.backfill'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                1
            );
            Ok(())
        })
        .await
        .unwrap();
    let duplicate = Request {
        since: 91.,
        until: 110.,
        apply: true,
        client_id: "backfill-1234".into(),
    };
    assert!(
        backfill::run(&f.store, &f.config, duplicate, Authority::Owner, 200.)
            .await
            .is_err()
    );
    // Delivery evidence cannot be fabricated by this operation.
    assert_eq!(fridica::attention::sweep(&f.store, 1100.).await.unwrap(), 2);
    assert_eq!(fridica::attention::sweep(&f.store, 1100.).await.unwrap(), 0);
}
#[tokio::test]
async fn authorization_range_bounds_and_transaction_faults_cannot_partially_backfill() {
    let f = Fixture::new().await;
    let request = Request {
        since: 90.,
        until: 110.,
        apply: true,
        client_id: "backfill-1234".into(),
    };
    for authority in [Authority::DesktopReadOnly, Authority::System] {
        assert!(
            backfill::run(&f.store, &f.config, request.clone(), authority, 200.)
                .await
                .is_err()
        );
    }
    for (since, until) in [
        (f64::NAN, 110.),
        (90., f64::INFINITY),
        (110., 90.),
        (90., 201.),
    ] {
        assert!(backfill::run(
            &f.store,
            &f.config,
            Request {
                since,
                until,
                ..request.clone()
            },
            Authority::Owner,
            200.
        )
        .await
        .is_err());
    }
    f.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_backfill BEFORE INSERT ON audit WHEN NEW.action='obligations.backfill' BEGIN SELECT RAISE(ABORT,'fixture'); END;")?;Ok(())}).await.unwrap();
    assert!(
        backfill::run(&f.store, &f.config, request, Authority::Owner, 200.)
            .await
            .is_err()
    );
    f.store
        .call(|c| {
            assert_eq!(
                c.query_row("SELECT count(*) FROM obligations", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                c.query_row("SELECT sum(mentions_owner) FROM messages", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            c.execute_batch("DROP TRIGGER fail_backfill")?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(f.run(true).await["count"], 2);
}

#[tokio::test]
async fn oversized_selection_refuses_the_entire_batch_instead_of_truncating_it() {
    let f = Fixture::new().await;
    f.store.call(|c| {
        c.execute_batch("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1001)
            INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,source,received_at)
            SELECT 'bulk-'||x,'TTEAM','CROOM',CAST(90.0+x/10000.0 AS TEXT),'active','UALICE','<@UOWNER> historical','history',90 FROM n;")?;
        Ok(())
    }).await.unwrap();
    let request = Request {
        since: 90.,
        until: 91.,
        apply: true,
        client_id: "backfill-bulk".into(),
    };
    assert!(
        backfill::run(&f.store, &f.config, request, Authority::Owner, 200.)
            .await
            .is_err()
    );
    f.store
        .call(|c| {
            assert_eq!(
                c.query_row("SELECT count(*) FROM obligations", [], |r| r
                    .get::<_, i64>(0))?,
                0
            );
            assert_eq!(
                c.query_row("SELECT count(*) FROM audit", [], |r| r.get::<_, i64>(0))?,
                0
            );
            Ok(())
        })
        .await
        .unwrap();
}
