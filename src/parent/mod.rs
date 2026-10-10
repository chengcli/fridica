//! Parent adapter: rebuild owner rules and context for every stateless call.
//! Complete prompts, schemas and bounded CLI output live only in the private DB.
pub mod attachments;
pub mod cli;
pub mod prompts;
use crate::{
    config::Config,
    core::{
        delivery::AdapterFuture,
        parent::{Parent, ParentFailure, ParentRequest},
        time::Clock,
    },
    exec::process,
    store::Shared,
};
pub use fridica_core::parent::{context, schema};
use fridica_core::store::Store as _;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap, ffi::OsString, io::Write, path::PathBuf, sync::Arc, time::Duration,
};

/// Trusted host construction only. These values never come from model output.
/// No Debug implementation: inherited authentication values may be private.
#[derive(Clone)]
pub struct Options {
    pub codex: String,
    pub claude: String,
    pub temporary_root: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            codex: "codex".into(),
            claude: "claude".into(),
            temporary_root: std::env::temp_dir(),
            environment: std::env::vars_os().collect(),
        }
    }
}
/// The failure code of a parent call that lost a race to refresh the
/// backend's expired login: several calls at once, typically the
/// catch-up after a restart, each refresh the OAuth token and all but one
/// fail. Temporary: the turn is retried after [`AUTH_RETRY`], without
/// blocking the thread.
pub const AUTH_CONTENDED: &str = "parent_auth_contended";
/// Seconds before a turn whose call lost the login refresh is retried; the
/// backend says the refresh settles within a minute.
pub const AUTH_RETRY: f64 = 60.;

/// Until a parent call has gone through since startup, or since a call lost
/// the login refresh, calls go one at a time, so only one of them refreshes
/// an expired login; then they run in parallel again.
struct LoginGate {
    open: std::sync::atomic::AtomicBool,
    probe: tokio::sync::Mutex<()>,
}
impl LoginGate {
    /// `Some` while this call goes alone; `None` once the gate is open, or
    /// after waiting `wait` ([`LOGIN_WAIT`]) for the call ahead, so waiting
    /// never costs a turn its own timeout (a lost refresh is retried anyway).
    async fn enter(&self, wait: Duration) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        use std::sync::atomic::Ordering::Acquire;
        if self.open.load(Acquire) {
            return None;
        }
        let probe = tokio::time::timeout(wait, self.probe.lock()).await.ok()?;
        (!self.open.load(Acquire)).then_some(probe)
    }
    /// A call that got through opens the gate; one that lost the refresh
    /// closes it again.
    fn settle(&self, contended: bool) {
        self.open
            .store(!contended, std::sync::atomic::Ordering::Release);
    }
}
/// The longest a parent call waits for the one ahead to get through.
const LOGIN_WAIT: Duration = Duration::from_secs(30);

pub struct CliParent {
    config: Arc<Config>,
    store: Shared,
    clock: Arc<dyn Clock>,
    options: Options,
    login: LoginGate,
}
fn failure(code: &str) -> ParentFailure {
    ParentFailure { code: code.into() }
}
impl CliParent {
    pub fn with_attachments<D: crate::slack::files::Downloader>(
        self,
        files: Arc<D>,
    ) -> attachments::WithAttachments<Self, D> {
        let (config, store, clock) = (self.config.clone(), self.store.clone(), self.clock.clone());
        attachments::WithAttachments::new(Arc::new(self), files, config, store, clock)
    }
    pub fn with_slack_context<
        D: crate::slack::files::Downloader + crate::slack::links::Reader + 'static,
    >(
        self,
        slack: Arc<D>,
    ) -> attachments::WithAttachments<Self, D> {
        self.with_attachments(slack.clone()).with_links(slack)
    }
    pub fn new(
        config: Arc<Config>,
        store: Shared,
        clock: Arc<dyn Clock>,
        options: Options,
    ) -> Self {
        Self {
            config,
            store,
            clock,
            options,
            login: LoginGate {
                open: std::sync::atomic::AtomicBool::new(false),
                probe: tokio::sync::Mutex::new(()),
            },
        }
    }
    /// Let parent calls run in parallel from the start, as if one had already
    /// gone through: for callers that know the backend's login is fresh.
    pub fn assume_logged_in(&self) {
        self.login.settle(false);
    }
    async fn run(&self, mut request: ParentRequest) -> Result<Value, ParentFailure> {
        request.session["now"] = json!(self.clock.now());
        let config = self.config.clone();
        let built = tokio::task::spawn_blocking(move || prompts::build(&config, &request))
            .await
            .map_err(|_| failure("parent_context_failed"))?
            .map_err(|_| failure("parent_context_failed"))?;
        let (prompt, schema, model) = built;
        if prompt.len() > process::OUTPUT_LIMIT {
            return Err(failure("parent_context_too_large"));
        }
        let mut temporary = tempfile::Builder::new();
        temporary.prefix("fridica-parent-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = temporary
            .tempdir_in(&self.options.temporary_root)
            .map_err(|_| failure("parent_temporary_directory_failed"))?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(directory.path().join("schema.json"))
            .and_then(|mut file| file.write_all(schema.to_string().as_bytes()))
            .map_err(|_| failure("parent_schema_write_failed"))?;
        let backend = &self.config.parent.backend;
        let executable = if backend == "codex" {
            &self.options.codex
        } else {
            &self.options.claude
        };
        let argv = cli::command(
            backend,
            executable,
            directory.path(),
            &schema,
            &model,
            &self.config.parent.reasoning_effort,
        )?;
        let timeout = Duration::try_from_secs_f64(self.config.parent.timeout)
            .map_err(|_| failure("parent_invalid_timeout"))?;
        let excluded = self.config.secret_env().map(str::to_owned);
        let environment = process::scrubbed_environment(
            self.options.environment.clone(),
            &excluded,
            &BTreeMap::new(),
        );
        let now = self.clock.now();
        let intent = json!({"backend":backend,"argv":argv,"prompt":prompt,"schema":schema,"model":model,"timeout":timeout.as_secs_f64()});
        let seq = self
            .store
            .transact(move |u| u.record("parent_transport_call", now, &intent.to_string(), false))
            .await
            .map_err(|_| failure("parent_recording_failed"))?;
        let probe = self.login.enter(LOGIN_WAIT).await;
        let completed = cli::execute(argv, directory.path(), environment, prompt, timeout).await;
        let contended = completed
            .as_ref()
            .is_ok_and(|output| cli::auth_contended(backend, &output.stdout));
        self.login.settle(contended);
        drop(probe);
        let (result, output, complete) = match completed {
            Ok(output) => {
                let result = if cli::rate_limited(backend, &output.stdout) {
                    Err(failure(crate::core::failure::RATE_LIMITED))
                } else if contended {
                    Err(failure(AUTH_CONTENDED))
                } else if output.returncode != 0 {
                    Err(failure(&format!("parent_exit_{}", output.returncode)))
                } else {
                    cli::parse(backend, &output.stdout)
                };
                (
                    result,
                    json!({"returncode":output.returncode,"stdout":output.stdout,"stderr":output.stderr}),
                    true,
                )
            }
            Err(error) => (Err(error), Value::Null, false),
        };
        let now = self.clock.now();
        let record = json!({"call_id":seq,"output":output,"result":result});
        self.store
            .transact(move |u| {
                u.complete(seq, complete)?;
                u.record(
                    "parent_transport_result",
                    now,
                    &record.to_string(),
                    complete,
                )?;
                Ok(())
            })
            .await
            .map_err(|_| failure("parent_recording_failed"))?;
        result
    }
}
impl Parent for CliParent {
    fn decide(&self, request: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(self.run(request))
    }
}

#[cfg(test)]
mod login_tests {
    use super::*;
    fn gate() -> &'static LoginGate {
        Box::leak(Box::new(LoginGate {
            open: std::sync::atomic::AtomicBool::new(false),
            probe: tokio::sync::Mutex::new(()),
        }))
    }

    /// One call goes alone until it gets through; the rest then run in
    /// parallel, until a call loses the refresh and closes the gate again.
    #[tokio::test]
    async fn one_call_refreshes_the_login_while_the_others_wait() {
        let g = gate();
        let first = g.enter(LOGIN_WAIT).await;
        assert!(first.is_some());
        let second = tokio::spawn(async move { g.enter(LOGIN_WAIT).await.is_some() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!second.is_finished());
        g.settle(false);
        drop(first);
        assert!(!second.await.unwrap());
        assert!(g.enter(LOGIN_WAIT).await.is_none());
        g.settle(true);
        assert!(g.enter(LOGIN_WAIT).await.is_some());
    }

    /// Waiting never costs a call its own timeout: after the wait it goes
    /// ahead without the gate.
    #[tokio::test]
    async fn a_call_waits_for_the_one_ahead_only_so_long() {
        let g = gate();
        let _first = g.enter(LOGIN_WAIT).await.unwrap();
        let wait = Duration::from_millis(100);
        let started = std::time::Instant::now();
        assert!(g.enter(wait).await.is_none());
        assert!(started.elapsed() >= wait);
    }
}
