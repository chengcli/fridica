//! Fixed read-only REST operations through the owner's existing gh authentication.
//! No model-provided endpoint, shell, cwd, header or method reaches the subprocess.
use crate::{
    config::Config,
    core::{delivery::AdapterFuture, time::Clock},
    exec::process::{self, Launch},
    store::Store,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    Issue { number: u64 },
    Pull { number: u64 },
    Branch { name: String },
    Tree { head: String },
    Compare { base: String, head: String },
    Checks { head: String, page: usize },
    Reviews { number: u64, page: usize },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub repo: String,
    pub operation: Operation,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum Failure {
    Invalid,
    NotFound,
    Authentication,
    Http { status: u16 },
    RateLimited { after: f64 },
    Unavailable,
    Recording,
}
impl Failure {
    pub fn message(&self) -> String {
        match self {
            Self::Invalid => "unexpected response from GitHub".into(),
            Self::NotFound => {
                "not found (missing or inaccessible to the authenticated owner)".into()
            }
            Self::Authentication => {
                "GitHub owner authentication is unavailable; check gh auth status".into()
            }
            Self::Http { status } => format!("HTTP {status}"),
            Self::RateLimited { .. } => "not fetched: GitHub rate limit".into(),
            Self::Unavailable => "could not reach GitHub".into(),
            Self::Recording => "GitHub context recording failed".into(),
        }
    }
}
pub trait Api: Send + Sync {
    fn get(&self, request: Request) -> AdapterFuture<'_, Result<Value, Failure>>;
}
pub fn repository(value: &str) -> bool {
    let Some((owner, repo)) = value.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && owner.len() <= 39
        && owner.as_bytes()[0].is_ascii_alphanumeric()
        && owner
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || v == b'-')
        && !repo.is_empty()
        && repo.len() <= 100
        && !matches!(repo, "." | "..")
        && repo
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || b"._-".contains(&v))
}
pub fn sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|v| v.is_ascii_hexdigit())
}
pub fn branch(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !value.ends_with('.')
        && !["..", "@{"].iter().any(|part| value.contains(*part))
        && value
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
        && value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || b"._-/".contains(&v))
}
fn encode(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
impl Request {
    pub fn endpoint(&self) -> Result<String, Failure> {
        if !repository(&self.repo) {
            return Err(Failure::Invalid);
        }
        let number = |n: u64| {
            if (1..=999_999_999).contains(&n) {
                Ok(n)
            } else {
                Err(Failure::Invalid)
            }
        };
        let head = |s: &str| {
            if sha(s) {
                Ok(())
            } else {
                Err(Failure::Invalid)
            }
        };
        let page = |p: usize| {
            if (1..=3).contains(&p) {
                Ok(p)
            } else {
                Err(Failure::Invalid)
            }
        };
        let suffix = match &self.operation {
            Operation::Issue { number: n } => format!("issues/{}", number(*n)?),
            Operation::Pull { number: n } => format!("pulls/{}", number(*n)?),
            Operation::Branch { name } => {
                if !branch(name) {
                    return Err(Failure::Invalid);
                }
                format!("git/ref/heads/{}", encode(name))
            }
            Operation::Tree { head: h } => {
                head(h)?;
                format!("git/commits/{h}")
            }
            Operation::Compare { base, head: h } => {
                head(h)?;
                if base.is_empty()
                    || base.len() > 255
                    || base.contains("..")
                    || base.contains(['\0', '\r', '\n'])
                {
                    return Err(Failure::Invalid);
                }
                format!("compare/{}...{h}", encode(base))
            }
            Operation::Checks { head: h, page: p } => {
                head(h)?;
                format!("commits/{h}/check-runs?per_page=100&page={}", page(*p)?)
            }
            Operation::Reviews { number: n, page: p } => format!(
                "pulls/{}/reviews?per_page=100&page={}",
                number(*n)?,
                page(*p)?
            ),
        };
        Ok(format!("/repos/{}/{suffix}", self.repo))
    }
}
#[derive(Clone)]
pub struct Options {
    pub program: PathBuf,
    pub timeout: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            program: "gh".into(),
            timeout: Duration::from_secs(8),
        }
    }
}
pub struct Gh {
    store: Store,
    clock: Arc<dyn Clock>,
    options: Options,
    env: BTreeMap<OsString, OsString>,
    secrets: Vec<String>,
    permits: Arc<tokio::sync::Semaphore>,
}
fn environment(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    token_env: &str,
) -> (BTreeMap<OsString, OsString>, Vec<String>) {
    let original: BTreeMap<_, _> = inherited.into_iter().collect();
    let mut env: BTreeMap<_, _> = original
        .iter()
        .filter(|(k, _)| {
            matches!(
                k.to_str(),
                Some(
                    "HOME"
                        | "PATH"
                        | "XDG_CONFIG_HOME"
                        | "GH_CONFIG_DIR"
                        | "GH_TOKEN"
                        | "GITHUB_TOKEN"
                        | "LANG"
                        | "LC_ALL"
                        | "SSL_CERT_FILE"
                        | "SSL_CERT_DIR"
                        | "HTTPS_PROXY"
                        | "HTTP_PROXY"
                        | "NO_PROXY"
                        | "https_proxy"
                        | "http_proxy"
                        | "no_proxy"
                )
            )
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // Preserve the legacy optional credential as an existing owner credential.
    // Never request a new token, and never put any token on argv or in a record.
    if !env.contains_key(std::ffi::OsStr::new("GH_TOKEN"))
        && !env.contains_key(std::ffi::OsStr::new("GITHUB_TOKEN"))
    {
        if let Some(token) = original.get(std::ffi::OsStr::new(token_env)) {
            env.insert("GH_TOKEN".into(), token.clone());
        }
    }
    for (k, v) in [
        ("GH_PROMPT_DISABLED", "1"),
        ("GH_NO_UPDATE_NOTIFIER", "1"),
        ("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1"),
        ("NO_COLOR", "1"),
        ("GH_PAGER", "cat"),
    ] {
        env.insert(k.into(), v.into());
    }
    let mut secrets: Vec<String> = original
        .iter()
        .filter(|(k, _)| {
            k.to_str()
                .is_some_and(|k| k == token_env || k.contains("TOKEN") || k.contains("SECRET"))
        })
        .filter_map(|(_, v)| v.to_str().filter(|v| !v.is_empty()).map(str::to_owned))
        .collect();
    for (key, value) in &env {
        if key
            .to_string_lossy()
            .to_ascii_lowercase()
            .ends_with("_proxy")
        {
            if let Some(value) = value.to_str() {
                if let Ok(url) = reqwest::Url::parse(value) {
                    if let Some(password) = url.password().filter(|s| !s.is_empty()) {
                        secrets.push(value.to_owned());
                        secrets.push(password.to_owned());
                    }
                }
            }
        }
    }
    (env, secrets)
}
impl Gh {
    pub fn new(
        config: &Config,
        store: Store,
        clock: Arc<dyn Clock>,
        options: Options,
    ) -> Result<Self, Failure> {
        Self::with_environment(config, store, clock, options, std::env::vars_os())
    }
    /// Explicit owner environment snapshot for deterministic daemon construction.
    pub fn with_environment(
        config: &Config,
        store: Store,
        clock: Arc<dyn Clock>,
        options: Options,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Result<Self, Failure> {
        if options.program.as_os_str().is_empty()
            || options.timeout.is_zero()
            || options.timeout > Duration::from_secs(30)
        {
            return Err(Failure::Invalid);
        }
        let (env, secrets) = environment(inherited, &config.github.token_env);
        Ok(Self {
            store,
            clock,
            options,
            env,
            secrets,
            permits: Arc::new(tokio::sync::Semaphore::new(4)),
        })
    }
    async fn run(&self, request: Request) -> Result<Value, Failure> {
        let endpoint = request.endpoint()?;
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| Failure::Unavailable)?;
        let now = self.clock.now();
        if !now.is_finite() {
            return Err(Failure::Invalid);
        }
        let argv = vec![
            self.options.program.to_string_lossy().into_owned(),
            "api".into(),
            "--hostname".into(),
            "github.com".into(),
            "--method".into(),
            "GET".into(),
            "--include".into(),
            "--header".into(),
            "Accept: application/vnd.github+json".into(),
            "--header".into(),
            "X-GitHub-Api-Version: 2022-11-28".into(),
            endpoint,
        ];
        let payload =
            json!({"request":request,"argv":argv,"timeout_ms":self.options.timeout.as_millis()});
        let (call,paused)=self.store.call(move|c|{
            let tx=c.transaction()?;
            let until:Option<String>=tx.query_row("SELECT value FROM meta WHERE key='github:read:pause_until'",[],|r|r.get(0)).optional()?;
            let until=until.and_then(|v|v.parse::<f64>().ok()).filter(|v|v.is_finite()).unwrap_or(0.);
            if until>now {
                tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('github_api_deferred',?,?)",params![now,json!({"request":payload,"until":until}).to_string()])?;
                tx.commit()?;return Ok((None,until-now));
            }
            tx.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('github_api_call',?,?,0)",params![now,payload.to_string()])?;
            let id=tx.last_insert_rowid();tx.commit()?;Ok((Some(id),0.))
        }).await.map_err(|_|Failure::Recording)?;
        let Some(call) = call else {
            return Err(Failure::RateLimited { after: paused });
        };
        // An empty private cwd prevents repository-local gh placeholders/configuration.
        let dir = tempfile::tempdir().map_err(|_| Failure::Unavailable)?;
        let output = process::run_once_with_permit(
            Launch {
                argv,
                cwd: Some(dir.path().into()),
                env: self.env.clone(),
            },
            vec![],
            self.options.timeout,
            process::OUTPUT_LIMIT,
            permit,
        )
        .await;
        let (result, record, complete) = match output {
            Ok(output) => {
                let raw = redact(&String::from_utf8_lossy(&output.stdout), &self.secrets);
                let decoded = if std::str::from_utf8(&output.stdout).is_ok() {
                    parse(&raw)
                } else {
                    Err(Failure::Invalid)
                };
                let result = if output.returncode == 4 {
                    Err(Failure::Authentication)
                } else {
                    match &decoded {
                        Ok((status, headers, body)) => {
                            if *status == 404 {
                                Err(Failure::NotFound)
                            } else if *status == 429
                                || (*status == 403
                                    && (headers
                                        .get("x-ratelimit-remaining")
                                        .is_some_and(|s| s == "0")
                                        || headers.contains_key("retry-after")))
                            {
                                Err(Failure::RateLimited {
                                    after: retry_after(headers, self.clock.now()),
                                })
                            } else if *status >= 400 {
                                Err(Failure::Http { status: *status })
                            } else if *status != 200 || output.returncode != 0 {
                                Err(Failure::Unavailable)
                            } else {
                                Ok(body.clone())
                            }
                        }
                        Err(e) => Err(e.clone()),
                    }
                };
                let record = json!({"call":call,"returncode":output.returncode,"stdout":raw,"stderr":redact(&String::from_utf8_lossy(&output.stderr),&self.secrets),"result":result});
                (result, record, true)
            }
            Err(_) => (
                Err(Failure::Unavailable),
                json!({"call":call,"error":"process_failed_or_output_incomplete"}),
                false,
            ),
        };
        let now = self.clock.now();
        let pause = match &result {
            Err(Failure::RateLimited { after }) => Some(now + after),
            _ => None,
        };
        self.store.call(move|c|{let tx=c.transaction()?;
            if let Some(until)=pause {tx.execute("INSERT INTO meta(key,value) VALUES('github:read:pause_until',?) ON CONFLICT(key) DO UPDATE SET value=MAX(CAST(value AS REAL),CAST(excluded.value AS REAL))",[until.to_string()])?;}tx.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('github_api_result',?,?,?)",params![now,record.to_string(),complete])?;if complete{tx.execute("UPDATE replay_events SET complete=1 WHERE seq=?",[call])?;}tx.commit()?;Ok(())}).await.map_err(|_|Failure::Recording)?;
        result
    }
}
impl Api for Gh {
    fn get(&self, request: Request) -> AdapterFuture<'_, Result<Value, Failure>> {
        Box::pin(self.run(request))
    }
}
fn redact(value: &str, secrets: &[String]) -> String {
    let mut value = value.to_owned();
    let mut secrets: Vec<_> = secrets.iter().collect();
    secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for secret in secrets {
        if !secret.is_empty() {
            value = value.replace(secret, "[redacted]");
        }
    }
    // One linear scan: repeated token-like strings in an untrusted body must
    // not turn redaction into quadratic work that blocks the async executor.
    static TOKENS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?:gh[pousr]_|github_pat_|xox[pb]-|xapp-|xoxe-)[A-Za-z0-9_-]*").unwrap()
    });
    TOKENS.replace_all(&value, "[redacted]").into_owned()
}

pub fn retry_after(headers: &BTreeMap<String, String>, now: f64) -> f64 {
    for (key, relative) in [("retry-after", true), ("x-ratelimit-reset", false)] {
        if let Some(n) = headers
            .get(key)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
        {
            return (if relative { n } else { n - now }).clamp(1., 3600.);
        }
    }
    300.
}
type Response = (u16, BTreeMap<String, String>, Value);
fn parse(raw: &str) -> Result<Response, Failure> {
    let raw = raw.replace("\r\n", "\n");
    let (header, body) = raw.split_once("\n\n").ok_or(Failure::Invalid)?;
    let mut lines = header.lines();
    let status = lines
        .next()
        .filter(|v| v.starts_with("HTTP/"))
        .and_then(|v| v.split_whitespace().nth(1))
        .and_then(|v| v.parse::<u16>().ok())
        .ok_or(Failure::Invalid)?;
    let headers = lines
        .filter_map(|v| v.split_once(':'))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().into()))
        .collect();
    let body = if status >= 400 {
        serde_json::from_str(body).unwrap_or(Value::Null)
    } else {
        serde_json::from_str(body).map_err(|_| Failure::Invalid)?
    };
    Ok((status, headers, body))
}

#[cfg(test)]
#[path = "../../tests/support/github_client.rs"]
mod tests;
