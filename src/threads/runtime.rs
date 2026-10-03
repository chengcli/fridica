//! Runtime composition with injected external adapters. Socket/control servers
//! can drive bounded passes; the database, not notifications, owns queued work.
use super::{actor, controls, dispatcher::Dispatcher, manager::Manager};
use crate::machines::probe::Monitor;
use crate::{
    approvals::Broker,
    attention::{self, Message},
    config::{editor, Config, LoadContext},
    core::{
        delivery::Delivery,
        parent::Parent,
        time::{Clock, Identifiers},
        Authority,
    },
    store::{configuration, outbox, work, Store},
    workers::{
        protocol::{Factory, JobIo},
        supervisor::{Options, Supervisor},
    },
};
use anyhow::{bail, Result};
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::Mutex;

pub struct Adapters<P: Parent, D: Delivery> {
    pub parent: Arc<P>,
    pub delivery: Arc<D>,
    pub workers: Arc<dyn Factory>,
    pub job_io: Arc<dyn JobIo>,
    /// Load readings for placement; None disables probing (tests, observe-only).
    pub machine_load: Option<Arc<Monitor>>,
}
#[derive(Debug, Default, PartialEq)]
pub struct Progress {
    pub turns: usize,
    pub started: usize,
    pub delivered: usize,
}
/// Trusted host factory: constructs adapters only; never starts external I/O.
pub type ParentFactory<P> = Arc<dyn Fn(Arc<Config>) -> Result<Arc<P>> + Send + Sync>;
struct Snapshot<P: Parent> {
    config: Arc<Config>,
    manager: Arc<Manager<P>>,
}
/// Seconds between archive rounds, and what one round moves at most.
const ARCHIVE_INTERVAL: f64 = 60.;
const ARCHIVE_THREADS: usize = 20;
const ARCHIVE_EVENTS: usize = 1000;
pub struct Runtime<P: Parent, D: Delivery> {
    store: Store,
    snapshot: RwLock<Snapshot<P>>,
    editing: Option<(ParentFactory<P>, LoadContext)>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn Identifiers>,
    supervisor: Supervisor,
    dispatcher: Dispatcher<D>,
    pub approvals: Arc<Broker>,
    machine_load: Option<Arc<Monitor>>,
    /// Owner-side reads of Slack text files (`fridica files get`).
    files: Option<Arc<dyn crate::slack::files::Downloader>>,
    pass: Mutex<()>,
    /// When the last archive round ran (#114).
    archived: std::sync::Mutex<f64>,
}
impl<P: Parent + 'static, D: Delivery> Runtime<P, D> {
    /// Lets owner controls read Slack text files through this client.
    pub fn with_files(mut self, files: Arc<dyn crate::slack::files::Downloader>) -> Self {
        self.files = Some(files);
        self
    }
    pub fn files(&self) -> Option<Arc<dyn crate::slack::files::Downloader>> {
        self.files.clone()
    }
    pub fn observe_only(&self) -> bool {
        self.dispatcher.observe_only
    }
    /// Startup only: caller supplies an exclusively opened Store and adapters
    /// whose constructors perform no external I/O. Recover before any startup.
    pub async fn start(
        store: Store,
        config: Arc<Config>,
        adapters: Adapters<P, D>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn Identifiers>,
        observe_only: bool,
    ) -> Result<Self> {
        adapters.workers.validate_config(&config)?;
        super::configuration::recover_startup(&store, &config, clock.now()).await?;
        let timeout = Duration::try_from_secs_f64(config.parent.timeout)?;
        work::recover(&store, clock.now()).await?;
        outbox::recover(&store, clock.now()).await?;
        actor::recover(&store).await?;
        // A database from before the channel ledger links its last week once (#108).
        let now = clock.now();
        store
            .call(move |c| {
                let tx = c.transaction()?;
                crate::store::links::backfill_tx(&tx, now, 7. * 86400.)?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        let approvals = Arc::new(Broker::new(
            store.clone(),
            config.clone(),
            clock.clone(),
            ids.clone(),
            None,
        ));
        let supervisor = Supervisor::new(
            store.clone(),
            config.clone(),
            adapters.workers,
            approvals.clone(),
            adapters.job_io,
            clock.clone(),
            Options {
                observe_only,
                ..Options::default()
            },
        )?;
        let actor = Arc::new(actor::Actor {
            store: store.clone(),
            config: Some(config.clone()),
            parent: adapters.parent,
            clock: clock.clone(),
            ids: ids.clone(),
            owner: config.owner.slack_user.clone(),
            limits: config.attention.clone(),
            observe_only,
            parent_timeout: timeout,
            machine_load: adapters.machine_load.clone(),
        });
        let manager = Manager::new(actor, config.limits.parent_concurrency)?;
        let dispatcher = Dispatcher {
            store: store.clone(),
            delivery: adapters.delivery,
            clock: clock.clone(),
            owner: config.owner.slack_user.clone(),
            observe_only,
            timeout: Duration::from_secs(30),
        };
        Ok(Self {
            store,
            snapshot: RwLock::new(Snapshot {
                config,
                manager: Arc::new(manager),
            }),
            editing: None,
            clock,
            ids,
            supervisor,
            dispatcher,
            approvals,
            machine_load: adapters.machine_load,
            files: None,
            pass: Mutex::new(()),
            archived: std::sync::Mutex::new(f64::NEG_INFINITY),
        })
    }
    pub fn store(&self) -> Store {
        self.store.clone()
    }
    pub fn config(&self) -> Arc<Config> {
        self.snapshot.read().unwrap().config.clone()
    }
    /// Install the host's pure adapter builder and original path-resolution context.
    pub fn with_configuration_editor(
        mut self,
        factory: ParentFactory<P>,
        context: LoadContext,
    ) -> Self {
        self.editing = Some((factory, context));
        self
    }
    /// Hosts with injected path/identity context must retain it for subsequent edits.
    pub fn with_configuration_context(mut self, context: LoadContext) -> Self {
        if let Some((_, current)) = &mut self.editing {
            *current = context;
        }
        self
    }
    pub fn configuration_editable(&self) -> bool {
        self.editing.is_some()
    }
    fn manager_for(&self, config: Arc<Config>) -> Result<Arc<Manager<P>>> {
        let (factory, _) = self
            .editing
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("configuration editing is unavailable"))?;
        let actor = Arc::new(actor::Actor {
            store: self.store.clone(),
            config: Some(config.clone()),
            parent: factory(config.clone())?,
            clock: self.clock.clone(),
            ids: self.ids.clone(),
            owner: config.owner.slack_user.clone(),
            limits: config.attention.clone(),
            observe_only: self.observe_only(),
            parent_timeout: Duration::try_from_secs_f64(config.parent.timeout)?,
            machine_load: self.machine_load.clone(),
        });
        Ok(Arc::new(Manager::new(
            actor,
            config.limits.parent_concurrency,
        )?))
    }
    async fn reconcile_configuration(&self) -> Result<()> {
        let Some((id, intent)) = configuration::pending(&self.store).await? else {
            return Ok(());
        };
        let current = self.config();
        let disk = editor::disk_fingerprint(&current.path)?;
        if intent.path != current.path {
            bail!("pending configuration path mismatch");
        }
        if disk == intent.before && current.fingerprint == intent.before {
            return configuration::complete(&self.store, id, false, self.now()).await;
        }
        if disk != intent.after {
            bail!("pending configuration conflicts with external changes");
        }
        if current.fingerprint != intent.after {
            let (_, context) = self.editing.as_ref().ok_or_else(|| {
                anyhow::anyhow!("configuration reconciliation requires adapter factory")
            })?;
            let config = Arc::new(crate::config::load(&current.path, context)?);
            super::configuration::verify(&intent, &config)?;
            let manager = self.manager_for(config.clone())?;
            self.supervisor.reconfigure(config.clone()).await?;
            *self.snapshot.write().unwrap() = Snapshot { config, manager };
        }
        editor::sync_directory(&current.path)?;
        configuration::complete(&self.store, id, true, self.now()).await
    }
    pub async fn update_configuration(
        &self,
        section: &str,
        changes: serde_json::Value,
        authority: Authority,
    ) -> Result<Arc<Config>> {
        if authority != Authority::Owner {
            bail!("configuration editing requires owner authentication");
        }
        let _pass = self.pass.lock().await;
        self.reconcile_configuration().await?;
        let current = self.config();
        let (_, context) = self
            .editing
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("configuration editing is unavailable"))?;
        let edit = editor::Prepared::new(&current, section, &changes, context)?;
        // Validate all prospective snapshots before recording or replacing files.
        self.manager_for(Arc::new(edit.config.clone()))?;
        self.supervisor
            .validate_reconfiguration(&edit.config)
            .await?;
        if edit.config.fingerprint == current.fingerprint {
            return Ok(current);
        }
        super::configuration::replace(&self.store, edit, self.now()).await?;
        self.reconcile_configuration().await?;
        Ok(self.config())
    }
    pub fn now(&self) -> f64 {
        self.clock.now()
    }
    pub async fn worker_control(&self, id: &str, stop: bool, authority: Authority) -> Result<bool> {
        if authority != Authority::Owner {
            bail!("worker controls require owner authentication");
        }
        if stop {
            self.supervisor.stop(id).await?;
            Ok(true)
        } else {
            self.supervisor.interrupt(id).await
        }
    }
    pub async fn processes(&self) -> std::collections::BTreeMap<String, String> {
        self.supervisor.processes().await
    }
    pub async fn instruct(
        &self,
        session: String,
        text: String,
        client_id: String,
        authority: Authority,
    ) -> Result<i64> {
        if self.observe_only() {
            bail!("instructions are unavailable in observe-only mode");
        }
        let lookup = session.clone();
        let (workspace, channel): (String, String) = self
            .store
            .call(move |c| {
                Ok(c.query_row(
                    "SELECT workspace,channel FROM threads WHERE id=?",
                    [lookup],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .await?;
        if workspace != self.config().slack.workspace
            || !self.config().slack.channels.contains(&channel)
        {
            bail!("instruction outside configured scope");
        }
        controls::instruct(
            &self.store,
            session,
            text,
            client_id,
            authority,
            self.clock.now(),
        )
        .await
    }
    async fn stop_closed_workers(&self) -> Result<()> {
        let now = self.clock.now();
        let pending: Vec<(i64,String)> = self.store.call(move |c| {
            let tx = c.transaction()?;
            let sessions: Vec<String> = tx.prepare("SELECT DISTINCT t.id FROM threads t JOIN workers w ON w.session_id=t.id WHERE t.control IN ('closed','archived','cleaned') AND w.status!='stopped'")?
                .query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            for session in sessions { controls::queue_stops_tx(&tx, &session, now, false)?; }
            let pending = tx.prepare("SELECT seq,json_extract(payload_json,'$.worker') FROM replay_events WHERE kind='thread_worker_stop' AND complete=0 ORDER BY seq")?
                .query_map([], |r| Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            tx.commit()?;
            Ok(pending)
        }).await?;
        for (intent, worker) in pending {
            self.supervisor.stop(&worker).await?;
            let now = self.clock.now();
            self.store.call(move |c| {
                let tx = c.transaction()?;
                tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('thread_worker_stopped',?,?)",
                    rusqlite::params![now,serde_json::json!({"call":intent,"worker":worker}).to_string()])?;
                tx.execute("UPDATE replay_events SET complete=1 WHERE seq=?", [intent])?;
                tx.commit()?;
                Ok(())
            }).await?;
        }
        Ok(())
    }
    /// Bind authenticated Slack envelopes to the same durable store and clock.
    pub fn slack_receiver(&self) -> crate::slack::receiver::Receiver {
        crate::slack::receiver::Receiver::new(
            self.store.clone(),
            self.config(),
            self.clock.clone(),
            self.ids.clone(),
        )
    }
    /// A socket adapter may acknowledge only after this durable intake returns.
    pub async fn intake(&self, message: Message) -> Result<Option<i64>> {
        if message.workspace != self.config().slack.workspace
            || !self.config().slack.channels.contains(&message.channel)
        {
            bail!("intake is outside configured Slack scope");
        }
        attention::intake(
            &self.store,
            message,
            self.config().owner.slack_user.clone(),
            self.clock.now(),
            self.config().attention.mention_grace,
            self.ids.next("obligation"),
        )
        .await
    }
    pub async fn control(
        &self,
        session: String,
        control: controls::Control,
        authority: Authority,
    ) -> Result<()> {
        controls::apply(&self.store, session, control, authority, self.clock.now()).await
    }
    pub async fn pass(&self) -> Result<Progress> {
        let _pass = self.pass.lock().await;
        self.reconcile_configuration().await?;
        self.supervisor.settle().await?;
        self.stop_closed_workers().await?;
        self.supervisor.reconcile_parent_controls().await?;
        attention::sweep(&self.store, self.clock.now()).await?;
        let manager = self.snapshot.read().unwrap().manager.clone();
        let turns = manager.sweep().await?;
        self.supervisor.reconcile_parent_controls().await?;
        let deny = self.config().egress.deny_list.clone();
        let delivered = self.dispatcher.drain_checked(100, deny.as_deref()).await?;
        let started = self.supervisor.schedule().await?.len();
        self.archive().await?;
        Ok(Progress {
            turns,
            started,
            delivered,
        })
    }
    /// Move quiet threads and old completed events to their weekly archives
    /// (#114), at most once a minute and in small rounds, so the live database
    /// stays small without stalling the database thread.
    pub async fn archive(&self) -> Result<crate::store::archive::Round> {
        let now = self.clock.now();
        {
            let mut last = self.archived.lock().unwrap();
            if now - *last < ARCHIVE_INTERVAL {
                return Ok(Default::default());
            }
            *last = now;
        }
        let config = self.config();
        let db = config.state.path.clone();
        let limits = crate::store::archive::Limits {
            threads_after: config.state.archive_after_days * 86400.,
            threads: ARCHIVE_THREADS,
            events_after: config.state.archive_events_after_hours * 3600.,
            events: ARCHIVE_EVENTS,
        };
        let result = self
            .store
            .call(move |c| crate::store::archive::round(c, &db, now, limits))
            .await;
        // Archiving is housekeeping: a failure (a full disk, an unwritable or
        // damaged archive) is noted for the owner, at most hourly, and never
        // stops the daemon's work.
        Ok(match result {
            Ok(round) => round,
            Err(error) => {
                let details = serde_json::json!({"error":error.to_string()}).to_string();
                let _ = self
                    .store
                    .call(move |c| {
                        c.execute(
                            "INSERT INTO health_events(kind,details_json,created) SELECT 'archive_failed',?,?
                             WHERE NOT EXISTS(SELECT 1 FROM health_events WHERE kind='archive_failed' AND created>?)",
                            rusqlite::params![details, now, now - 3600.],
                        )?;
                        Ok(())
                    })
                    .await;
                Default::default()
            }
        })
    }
    pub async fn close(&self) -> Result<()> {
        let _pass = self.pass.lock().await;
        self.supervisor.close().await
    }
}

impl<P: Parent + 'static> Runtime<P, crate::slack::web::SlackClient> {
    /// Build the live service with one authenticated client for history and
    /// delivery, and the runtime's receiver for Socket Mode intake.
    pub fn slack_service(
        self,
        app_token: String,
        socket_options: crate::slack::socket::Options,
        options: super::service::Options,
    ) -> Result<
        super::service::Service<P, crate::slack::web::SlackClient, crate::slack::web::SlackClient>,
        super::service::Failure,
    > {
        let web = self.dispatcher.delivery.clone();
        super::service::Service::new(
            self,
            web.clone(),
            move |receiver| {
                let client = Arc::new(crate::slack::web::WebClient::clone(&web));
                crate::slack::socket::socket_mode(client, receiver, app_token, socket_options)
            },
            options,
        )
    }
}
