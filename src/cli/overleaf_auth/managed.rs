//! Owner-only acquisition. All remote data/errors stay private; no Debug on secrets.
//! CDP is carried by anonymous pipes, never a TCP debugging endpoint.
use super::ENTRY;
use crate::{
    core::secret::SecretStore,
    exec::process::{Launch, Process},
    secrets::NativeStore,
};
use anyhow::{anyhow, ensure, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
};

const PROJECT: &str = "6b0483a71b304b7fd2141775";
const ORIGIN: &str = "https://www.overleaf.com";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const COOKIE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const LIMIT: usize = 1024 * 1024;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn account_id(value: &str) -> Result<()> {
    ensure!(
        value.len() == 24
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "expected a lowercase 24-digit existing Overleaf account ID"
    );
    Ok(())
}

fn email(value: &str) -> Result<String> {
    ensure!(
        value.len() <= 254
            && value.is_ascii()
            && !value.bytes().any(|c| c <= 32 || c == 127)
            && value.split('@').count() == 2
            && value.split('@').all(|s| !s.is_empty()),
        "expected the existing Overleaf account email"
    );
    Ok(value.to_ascii_lowercase())
}
pub(super) async fn sign_in(expected: &str, expires_at: Option<u64>) -> Result<()> {
    email(expected)?;
    ensure!(
        cfg!(target_os = "macos"),
        "managed Overleaf sign-in requires macOS; no browser or secret store was accessed"
    );
    let browser = Path::new("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
    ensure!(
        browser.is_file(),
        "managed sign-in requires Google Chrome in /Applications"
    );
    let http = Bootstrap::production()?;
    // SIGINT/SIGTERM and ordinary failures all pass through cleanup before returning.
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|_| anyhow!("cannot install managed sign-in termination handler"))?;
    let mut browser = Browser::start(&[browser.to_string_lossy().into_owned()], None)?;
    let acquire = async {
        tokio::select! {
            result = browser.acquire(expected, &http, expires_at) => result,
            _ = tokio::signal::ctrl_c() => Err(anyhow!("managed sign-in cancelled")),
            _ = terminate.recv() => Err(anyhow!("managed sign-in cancelled")),
        }
    };
    let result = tokio::time::timeout(LOGIN_TIMEOUT, acquire)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow!(
                "managed sign-in timed out before account/project verification completed"
            ))
        });
    finish(&mut browser, &NativeStore, result).await
}

async fn finish(
    browser: &mut Browser,
    store: &dyn SecretStore,
    result: Result<String>,
) -> Result<()> {
    // Kill/reap only our process group, then delete only our generated temporary profile.
    // A cleanup failure must never turn into a usable saved credential.
    browser.close().await?;
    let envelope = result?;
    crate::config::overleaf::validate_session(&envelope)
        .map_err(|_| anyhow!("acquired native session is invalid or expired"))?;
    store.set(ENTRY, envelope.as_bytes())
        .map_err(|_| anyhow!("native secret save failed; dedicated entry state is uncertain; inspect status or revoke before retrying"))
}

struct Browser {
    process: Process,
    profile: Option<TempDir>,
    cdp: Cdp,
}
impl Browser {
    fn start(program: &[String], parent: Option<&Path>) -> Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("fridica-overleaf-signin-");
        let profile = match parent {
            Some(path) => builder.tempdir_in(path),
            None => builder.tempdir(),
        }
        .map_err(|_| anyhow!("cannot create managed browser profile"))?;
        // POSIX shell only maps stdin/stdout to Chrome's documented CDP fds 3/4.
        // exec preserves the process-group leader. Browser stdout/stderr are discarded.
        let mut argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            "exec 3<&0 4>&1; exec \"$@\" </dev/null >/dev/null 2>/dev/null".into(),
            "fridica-managed-browser".into(),
        ];
        argv.extend_from_slice(program);
        argv.extend([
            format!("--user-data-dir={}", profile.path().display()),
            "--remote-debugging-pipe".into(),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-sync".into(),
            "--use-mock-keychain".into(),
            "--password-store=basic".into(),
            "--disable-extensions".into(),
            "--disable-background-networking".into(),
            "--disable-component-update".into(),
            "--disable-default-apps".into(),
            "--disable-breakpad".into(),
            "--disable-crash-reporter".into(),
            "--no-proxy-server".into(),
            "--app=about:blank".into(),
        ]);
        // Never inherit Git/native/Slack credentials, proxy settings or Chrome overrides.
        let env = BTreeMap::from([
            ("HOME".into(), profile.path().as_os_str().to_owned()),
            ("TMPDIR".into(), profile.path().as_os_str().to_owned()),
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("LANG".into(), "en_US.UTF-8".into()),
        ]);
        let mut process = Process::start(&Launch {
            argv,
            cwd: Some(profile.path().to_owned()),
            env,
        })
        .map_err(|_| anyhow!("managed browser failed to start"))?;
        let input = process
            .stdin()
            .ok_or_else(|| anyhow!("managed CDP input unavailable"))?;
        let output = process
            .stdout()
            .ok_or_else(|| anyhow!("managed CDP output unavailable"))?;
        Ok(Self {
            process,
            profile: Some(profile),
            cdp: Cdp {
                input,
                output: BufReader::new(output),
                next: 0,
                session: String::new(),
            },
        })
    }

    async fn acquire(
        &mut self,
        expected: &str,
        http: &Bootstrap,
        deadline: Option<u64>,
    ) -> Result<String> {
        email(expected)?;
        let target = tokio::time::timeout(IO_TIMEOUT, async {
            loop {
                let targets = self.cdp.call("Target.getTargets", json!({}), false).await?;
                let pages: Vec<_> = targets["targetInfos"]
                    .as_array()
                    .ok_or_else(|| anyhow!("managed browser targets unavailable"))?
                    .iter()
                    .filter(|t| t["type"] == "page")
                    .collect();
                if pages.is_empty() {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                ensure!(
                    pages.len() == 1 && pages[0]["url"] == "about:blank",
                    "managed browser did not start with one blank page"
                );
                return string(pages[0], "targetId");
            }
        })
        .await
        .map_err(|_| anyhow!("managed browser initial page timed out"))??;
        let attached = self
            .cdp
            .call(
                "Target.attachToTarget",
                json!({"targetId":target,"flatten":true}),
                false,
            )
            .await?;
        self.cdp.session = string(&attached, "sessionId")?;
        self.cdp
            .call(
                "Browser.setDownloadBehavior",
                json!({"behavior":"deny"}),
                false,
            )
            .await?;
        // The human-controlled browser may load normal site resources, dashboard
        // redirects and SSO. Data collection is scoped below, not by a URL firewall.
        let navigation = self
            .cdp
            .call(
                "Page.navigate",
                json!({"url":format!("{ORIGIN}/login")}),
                true,
            )
            .await?;
        ensure!(
            navigation.get("errorText").is_none(),
            "managed login navigation failed"
        );
        // Cookie polling is the only login observation. No page, navigation or
        // request payload is inspected. The caller bounds this loop by LOGIN_TIMEOUT.
        loop {
            let response = self
                .cdp
                .call(
                    "Network.getCookies",
                    json!({"urls":[format!("{ORIGIN}/project/{PROJECT}")]}),
                    true,
                )
                .await?;
            let cookies: Vec<Cookie> = serde_json::from_value(response["cookies"].clone())
                .map_err(|_| anyhow!("invalid managed cookie response"))?;
            if cookies.iter().any(|c| c.name == "overleaf_session2") {
                let (cookie, cookie_expiry) = scoped_cookies(cookies, now())?;
                if let Some(enrollment) = http.verify(&cookie, expected).await? {
                    let expiry = deadline
                        .unwrap_or(u64::MAX)
                        .min(cookie_expiry)
                        .min(enrollment.expires_at);
                    let expires_at = (expiry != u64::MAX).then_some(expiry);
                    let envelope = json!({"schema":2,"cookie":enrollment.cookie,"account_id":enrollment.account_id,"expected_email":email(expected)?,
                        "projects":[PROJECT],"operations":crate::config::overleaf::OPERATIONS,"expires_at":expires_at}).to_string();
                    crate::config::overleaf::validate_session(&envelope)
                        .map_err(|_| anyhow!("acquired native session is invalid or expired"))?;
                    return Ok(envelope);
                }
            }
            // A stable anonymous cookie can become authenticated server-side.
            tokio::time::sleep(COOKIE_POLL_INTERVAL).await;
        }
    }

    async fn close(&mut self) -> Result<()> {
        self.process
            .terminate(Duration::from_secs(1))
            .await
            .map_err(|_| anyhow!("managed browser cleanup could not confirm process exit"))?;
        if let Some(profile) = self.profile.take() {
            profile.close().map_err(|_| {
                anyhow!("managed browser exited but temporary profile removal failed")
            })?;
        }
        Ok(())
    }
}

// Process drops before profile, killing the owned group on unwind. Normal cancellation
// is handled above and waits for exit. SIGKILL/power loss cannot guarantee disk cleanup.
struct Cdp {
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next: u64,
    session: String,
}
fn string(value: &Value, key: &str) -> Result<String> {
    value[key]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 1024)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("invalid managed browser protocol identifier"))
}
impl Cdp {
    async fn send(&mut self, method: &str, params: Value, page: bool) -> Result<u64> {
        self.next += 1;
        let mut request = json!({"id":self.next,"method":method,"params":params});
        if page {
            request["sessionId"] = self.session.clone().into();
        }
        let mut bytes =
            serde_json::to_vec(&request).map_err(|_| anyhow!("managed CDP encoding failed"))?;
        bytes.push(0);
        self.input
            .write_all(&bytes)
            .await
            .map_err(|_| anyhow!("managed browser pipe closed"))?;
        Ok(self.next)
    }
    async fn call(&mut self, method: &str, params: Value, page: bool) -> Result<Value> {
        tokio::time::timeout(IO_TIMEOUT, async {
            let id = self.send(method, params, page).await?;
            loop {
                let message = self.receive().await?;
                if message.id == Some(id) {
                    ensure!(
                        message.error.is_none(),
                        "managed browser rejected a CDP command"
                    );
                    return message
                        .result
                        .ok_or_else(|| anyhow!("managed CDP result missing"));
                }
            }
        })
        .await
        .map_err(|_| anyhow!("managed browser command timed out"))?
    }
    async fn receive(&mut self) -> Result<Reply> {
        let mut bytes = Vec::new();
        (&mut self.output)
            .take((LIMIT + 1) as u64)
            .read_until(0, &mut bytes)
            .await
            .map_err(|_| anyhow!("managed browser pipe read failed"))?;
        ensure!(
            bytes.last() == Some(&0) && bytes.len() <= LIMIT,
            "managed browser closed or exceeded message limit"
        );
        // Unknown event fields (including params) and error contents are ignored
        // during deserialization. There are no event handlers or subscriptions.
        serde_json::from_slice(&bytes[..bytes.len() - 1])
            .map_err(|_| anyhow!("invalid managed CDP response"))
    }
}

#[derive(Deserialize)]
struct Reply {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cookie {
    name: String,
    value: String,
    domain: String,
    path: String,
    secure: bool,
    http_only: bool,
    session: bool,
    expires: f64,
    #[serde(default)]
    partition_key: Option<Value>,
}
fn scoped_cookies(cookies: Vec<Cookie>, clock: u64) -> Result<(String, u64)> {
    ensure!(
        cookies.len() <= 128,
        "managed cookie response exceeds limit"
    );
    let mut selected = BTreeMap::new();
    let mut expiry = u64::MAX;
    for cookie in cookies {
        if !matches!(cookie.name.as_str(), "overleaf_session2" | "GCLB") {
            continue;
        }
        ensure!(
            matches!(
                cookie.domain.as_str(),
                "www.overleaf.com" | ".www.overleaf.com" | "overleaf.com" | ".overleaf.com"
            ) && cookie.path == "/"
                && (cookie.name == "GCLB" || (cookie.secure && cookie.http_only))
                && cookie.partition_key.is_none(),
            "native cookie has unexpected origin, path or protection"
        );
        ensure!(
            !cookie.value.is_empty()
                && cookie.value.len() <= 4096
                && cookie
                    .value
                    .bytes()
                    .all(|b| (33..127).contains(&b) && !b";\"\\,".contains(&b)),
            "invalid native session cookie"
        );
        if cookie.session {
            ensure!(cookie.expires == -1.0, "inconsistent native session expiry");
        } else {
            ensure!(
                cookie.expires.is_finite() && cookie.expires > clock as f64,
                "native cookie is expired"
            );
            expiry = expiry.min(cookie.expires.floor() as u64);
        }
        ensure!(
            selected.insert(cookie.name, cookie.value).is_none(),
            "ambiguous native session cookies"
        );
    }
    ensure!(
        selected.contains_key("overleaf_session2"),
        "managed login did not produce a native session"
    );
    Ok((
        selected
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; "),
        expiry,
    ))
}

struct Enrollment {
    account_id: String,
    cookie: String,
    expires_at: u64,
}

struct Bootstrap {
    client: reqwest::Client,
    url: String,
}
impl Bootstrap {
    fn production() -> Result<Self> {
        Ok(Self {
            client: Self::client()?,
            url: format!("{ORIGIN}/project/{PROJECT}"),
        })
    }
    fn client() -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(IO_TIMEOUT)
            .build()
            .map_err(|_| anyhow!("native verification client unavailable"))
    }
    async fn verify(&self, cookie: &str, expected: &str) -> Result<Option<Enrollment>> {
        let expected = email(expected)?;
        let mut response = self
            .client
            .get(&self.url)
            .header("Cookie", cookie)
            .header("Accept", "text/html")
            .send()
            .await
            .map_err(|_| anyhow!("native account/project verification request failed"))?;
        // An anonymous session is normal while the human is still logging in.
        // Never follow the redirect or inspect a login response body.
        let login_redirect = response.status().is_redirection()
            && response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .and_then(|location| response.url().join(location).ok())
                .is_some_and(|url| {
                    url.origin().ascii_serialization() == ORIGIN
                        && url.path() == "/login"
                        && url.username().is_empty()
                        && url.password().is_none()
                });
        if response.status() == reqwest::StatusCode::UNAUTHORIZED || login_redirect {
            return Ok(None);
        }
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "native account/project verification rejected; redirects are not followed"
        );
        ensure!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.split(';').next() == Some("text/html")),
            "unexpected native verification response type"
        );
        let mut cookies: BTreeMap<String, Cookie> = cookie
            .split("; ")
            .filter_map(|p| p.split_once('='))
            .map(|(name, value)| {
                (
                    name.to_owned(),
                    Cookie {
                        name: name.into(),
                        value: value.into(),
                        domain: "www.overleaf.com".into(),
                        path: "/".into(),
                        secure: true,
                        http_only: true,
                        session: true,
                        expires: -1.0,
                        partition_key: None,
                    },
                )
            })
            .collect();
        for header in response.headers().get_all("set-cookie") {
            let raw = header
                .to_str()
                .map_err(|_| anyhow!("invalid native cookie rotation"))?;
            let name = raw.split('=').next().unwrap_or("").trim();
            if !matches!(name, "overleaf_session2" | "GCLB") {
                continue;
            }
            let rotated = cookie::Cookie::parse(raw.to_owned())
                .map_err(|_| anyhow!("invalid native cookie rotation"))?;
            let expires = if let Some(age) = rotated.max_age() {
                now() as f64 + age.whole_seconds() as f64
            } else {
                rotated
                    .expires_datetime()
                    .map_or(-1.0, |t| t.unix_timestamp() as f64)
            };
            if name == "GCLB" && expires != -1.0 && expires <= now() as f64 {
                cookies.remove(name);
                continue;
            }
            cookies.insert(
                name.into(),
                Cookie {
                    name: name.into(),
                    value: rotated.value().into(),
                    domain: rotated.domain().unwrap_or("www.overleaf.com").into(),
                    path: rotated.path().unwrap_or("/project").into(),
                    secure: rotated.secure().unwrap_or(false),
                    http_only: rotated.http_only().unwrap_or(false),
                    session: expires == -1.0,
                    expires,
                    partition_key: rotated.partitioned().unwrap_or(false).then(|| json!({})),
                },
            );
        }
        let (cookie, expires_at) = scoped_cookies(cookies.into_values().collect(), now())?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow!("native verification response unavailable"))?
        {
            ensure!(
                chunk.len() <= LIMIT.saturating_sub(body.len()),
                "native verification response exceeds limit"
            );
            body.extend_from_slice(&chunk);
        }
        let html = std::str::from_utf8(&body)
            .map_err(|_| anyhow!("invalid native verification response"))?;
        ensure!(
            meta(html, "ol-project_id")? == PROJECT,
            "native sign-in project mismatch"
        );
        let user: Value = serde_json::from_str(&meta(html, "ol-user")?)
            .map_err(|_| anyhow!("invalid native account metadata"))?;
        let account = user
            .get("_id")
            .or(user.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("native account ID missing"))?;
        account_id(account)?;
        let actual_email = user
            .get("email")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("native account email missing; cannot enroll identity"))?;
        ensure!(
            email(actual_email)? == expected,
            "native sign-in account email mismatch"
        );
        Ok(Some(Enrollment {
            account_id: account.into(),
            cookie,
            expires_at,
        }))
    }
}
fn meta(html: &str, name: &str) -> Result<String> {
    let tags = regex::Regex::new(r"(?is)<meta\s+[^>]*>").unwrap();
    let attrs = regex::Regex::new(r#"([\w-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap();
    let mut values = Vec::new();
    for tag in tags.find_iter(html) {
        let fields: BTreeMap<_, _> = attrs
            .captures_iter(tag.as_str())
            .map(|c| {
                (
                    c[1].to_owned(),
                    html_escape::decode_html_entities(c.get(2).or(c.get(3)).unwrap().as_str())
                        .into_owned(),
                )
            })
            .collect();
        if fields.get("name").is_some_and(|n| n == name) {
            values.push(
                fields
                    .get("content")
                    .cloned()
                    .ok_or_else(|| anyhow!("native verification metadata missing"))?,
            );
        }
    }
    ensure!(
        values.len() == 1,
        "native verification metadata missing or ambiguous"
    );
    Ok(values.remove(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_selects_only_applicable_session_cookies() {
        let make = |name: &str, domain: &str| Cookie {
            name: name.into(),
            value: "fixture".into(),
            domain: domain.into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            session: true,
            expires: -1.0,
            partition_key: None,
        };
        let (cookie, expiry) = scoped_cookies(
            vec![
                make("overleaf_session2", "www.overleaf.com"),
                make("analytics", "www.overleaf.com"),
            ],
            now(),
        )
        .unwrap();
        assert_eq!(cookie, "overleaf_session2=fixture");
        assert_eq!(expiry, u64::MAX);
        assert!(scoped_cookies(vec![make("overleaf_session2", "sso.example")], now()).is_err());
        assert!(scoped_cookies(
            vec![
                make("overleaf_session2", "www.overleaf.com"),
                make("overleaf_session2", ".overleaf.com")
            ],
            now()
        )
        .is_err());
    }
}
