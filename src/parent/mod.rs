//! Parent adapter: rebuild owner rules and context for every stateless call.
//! Complete prompts, schemas and bounded CLI output live only in the private DB.
pub mod attachments;
pub mod cli;
pub mod context;
pub mod prompts;
pub mod schema;
use crate::{
    config::Config,
    core::{
        delivery::AdapterFuture,
        parent::{Parent, ParentFailure, ParentRequest},
        time::Clock,
    },
    exec::process,
    store::Store,
};
use rusqlite::params;
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
pub struct CliParent {
    config: Arc<Config>,
    store: Store,
    clock: Arc<dyn Clock>,
    options: Options,
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
    pub fn new(config: Arc<Config>, store: Store, clock: Arc<dyn Clock>, options: Options) -> Self {
        Self {
            config,
            store,
            clock,
            options,
        }
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
        let seq=self.store.call(move|c|{c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('parent_transport_call',?,?,0)",params![now,intent.to_string()])?;Ok(c.last_insert_rowid())}).await.map_err(|_|failure("parent_recording_failed"))?;
        let completed = cli::execute(argv, directory.path(), environment, prompt, timeout).await;
        let (result, output, complete) = match completed {
            Ok(output) => {
                let result = if output.returncode != 0 {
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
        self.store.call(move|c|{let tx=c.transaction()?;
            tx.execute("UPDATE replay_events SET complete=? WHERE seq=?",params![complete,seq])?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('parent_transport_result',?,?,?)",params![now,record.to_string(),complete])?;
            tx.commit()?;Ok(())
        }).await.map_err(|_|failure("parent_recording_failed"))?;
        result
    }
}
impl Parent for CliParent {
    fn decide(&self, request: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(self.run(request))
    }
}
