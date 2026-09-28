//! Runtime composition with injected external adapters. Socket/control servers
//! can drive bounded passes; the database, not notifications, owns queued work.
use super::{actor, controls, manager::Manager};
use crate::{
    approvals::Broker,
    attention::{self, Message},
    config::Config,
    core::{
        delivery::Delivery,
        parent::Parent,
        time::{Clock, Identifiers},
        Authority,
    },
    slack::outbox::Dispatcher,
    store::{outbox, work, Store},
    workers::{
        protocol::{Factory, JobIo},
        supervisor::{Options, Supervisor},
    },
};
use anyhow::{bail, Result};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub struct Adapters<P: Parent, D: Delivery> {
    pub parent: Arc<P>,
    pub delivery: Arc<D>,
    pub workers: Arc<dyn Factory>,
    pub job_io: Arc<dyn JobIo>,
}
#[derive(Debug, Default, PartialEq)]
pub struct Progress {
    pub turns: usize,
    pub started: usize,
    pub delivered: usize,
}
pub struct Runtime<P: Parent, D: Delivery> {
    store: Store,
    config: Arc<Config>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn Identifiers>,
    manager: Manager<P>,
    supervisor: Supervisor,
    dispatcher: Dispatcher<D>,
    pub approvals: Arc<Broker>,
    pass: Mutex<()>,
}
impl<P: Parent + 'static, D: Delivery> Runtime<P, D> {
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
        let timeout = Duration::try_from_secs_f64(config.parent.timeout)?;
        work::recover(&store, clock.now()).await?;
        outbox::recover(&store, clock.now()).await?;
        actor::recover(&store).await?;
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
            config,
            clock,
            ids,
            manager,
            supervisor,
            dispatcher,
            approvals,
            pass: Mutex::new(()),
        })
    }
    pub fn store(&self) -> Store {
        self.store.clone()
    }
    pub fn config(&self) -> Arc<Config> {
        self.config.clone()
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
        if workspace != self.config.slack.workspace
            || !self.config.slack.channels.contains(&channel)
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
        let ids:Vec<String>=self.store.call(|c|Ok(c.prepare("SELECT w.id FROM workers w JOIN threads t ON t.id=w.session_id WHERE t.control IN ('closed','archived','cleaned') AND w.status!='stopped'")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?)).await?;
        for id in ids {
            self.supervisor.stop(&id).await?;
        }
        Ok(())
    }
    /// Bind authenticated Slack envelopes to the same durable store and clock.
    pub fn slack_receiver(&self) -> crate::slack::receiver::Receiver {
        crate::slack::receiver::Receiver::new(
            self.store.clone(),
            self.config.clone(),
            self.clock.clone(),
            self.ids.clone(),
        )
    }
    /// A socket adapter may acknowledge only after this durable intake returns.
    pub async fn intake(&self, message: Message) -> Result<Option<i64>> {
        if message.workspace != self.config.slack.workspace
            || !self.config.slack.channels.contains(&message.channel)
        {
            bail!("intake is outside configured Slack scope");
        }
        attention::intake(
            &self.store,
            message,
            self.config.owner.slack_user.clone(),
            self.clock.now(),
            self.config.attention.mention_grace,
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
        self.supervisor.settle().await?;
        self.stop_closed_workers().await?;
        attention::sweep(&self.store, self.clock.now()).await?;
        let turns = self.manager.sweep().await?;
        let delivered = self.dispatcher.drain(100).await?;
        let started = self.supervisor.schedule().await?.len();
        Ok(Progress {
            turns,
            started,
            delivered,
        })
    }
    pub async fn close(&self) -> Result<()> {
        let _pass = self.pass.lock().await;
        self.supervisor.close().await
    }
}

impl<P: Parent + 'static> Runtime<P, crate::slack::web::WebClient> {
    /// Build the live service with one authenticated client for history and
    /// delivery, and the runtime's receiver for Socket Mode intake.
    pub fn slack_service(
        self,
        app_token: String,
        socket_options: crate::slack::socket::Options,
        options: super::service::Options,
    ) -> Result<
        super::service::Service<P, crate::slack::web::WebClient, crate::slack::web::WebClient>,
        super::service::Failure,
    > {
        let web = self.dispatcher.delivery.clone();
        super::service::Service::new(
            self,
            web.clone(),
            move |receiver| {
                crate::slack::socket::SocketMode::new(web, receiver, app_token, socket_options)
            },
            options,
        )
    }
}
