//! Credential diagnostics also work before machine and workspace setup.
use super::checks::{Report, Status};
use crate::config::schema::{GitHub, Slack};
use std::{collections::BTreeMap, ffi::OsString, time::Duration};

pub(super) async fn check(
    report: &mut Report,
    slack: &Slack,
    github: &GitHub,
    environment: &BTreeMap<OsString, OsString>,
    timeout: Duration,
    live_slack: bool,
) {
    for (name, variable, reference, prefix, method) in [
        (
            "Slack app token",
            &slack.app_token_env,
            &slack.app_token,
            "xapp-",
            "apps.connections.open",
        ),
        (
            "Slack user token",
            &slack.user_token_env,
            &slack.user_token,
            "xoxp-",
            "auth.test",
        ),
    ] {
        let env = environment.get(std::ffi::OsStr::new(variable));
        let value = crate::secrets::resolve(
            &crate::secrets::NativeStore,
            variable,
            env.map(|s| s.as_os_str()),
            reference.as_ref(),
        );
        let source = if env.is_some() {
            format!("environment variable {variable}")
        } else if let Some(reference) = reference {
            format!("keyring entry {}", reference.name())
        } else {
            variable.clone()
        };
        match value {
            Err(error) => report.add(Status::Fail, name, format!("{source}: {error}")),
            Ok(token) => {
                let (status, detail) = if !token.starts_with(prefix)
                    || token.len() <= prefix.len()
                    || !token.bytes().all(|b| b.is_ascii_graphic())
                {
                    (Status::Fail, "wrong Slack token format")
                } else if live_slack {
                    verify(&token, &format!("https://slack.com/api/{method}"), timeout).await
                } else {
                    (Status::Pass, "expected token format")
                };
                report.add(status, name, format!("{source}: {detail}"));
            }
        }
    }
    if !github.enabled {
        report.add(Status::Skip, "GitHub reader", "github.enabled is off");
        return;
    }
    let present = |key: &str| environment.contains_key(std::ffi::OsStr::new(key));
    let source = if let Some(reference) = &github.token {
        if present(&github.token_env) {
            format!("environment variable {}", github.token_env)
        } else {
            format!("keyring entry {}", reference.name())
        }
    } else if let Some(key) = ["GH_TOKEN", "GITHUB_TOKEN", github.token_env.as_str()]
        .into_iter()
        .find(|key| present(key))
    {
        format!("environment variable {key}")
    } else {
        "GitHub CLI credentials".to_owned()
    };
    let options = crate::github::client::Options::default();
    match crate::github::client::probe(github, environment.clone(), &options, timeout).await {
        Ok(()) => report.add(
            Status::Pass,
            "GitHub reader",
            format!("{source}: github.com reader probe passed"),
        ),
        Err(error) => report.add(
            Status::Warn,
            "GitHub reader",
            format!("{source}: {}", error.message()),
        ),
    }
}

async fn verify(token: &str, url: &str, timeout: Duration) -> (Status, &'static str) {
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
    else {
        return (Status::Warn, "could not initialize Slack verification");
    };
    let Ok(response) = client.post(url).bearer_auth(token).send().await else {
        return (Status::Warn, "could not reach Slack to verify token");
    };
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return (Status::Fail, "Slack rejected the credential");
    }
    if !status.is_success() {
        return (Status::Warn, "Slack verification unavailable");
    }
    // Deserialize only the verdict. A Socket Mode response also carries a secret URL.
    #[derive(serde::Deserialize)]
    struct Verdict {
        ok: bool,
        error: Option<String>,
    }
    match response.json::<Verdict>().await {
        Ok(Verdict { ok: true, .. }) => (Status::Pass, "verified by Slack"),
        Ok(Verdict {
            error: Some(code), ..
        }) if matches!(
            code.as_str(),
            "invalid_auth"
                | "token_revoked"
                | "token_expired"
                | "account_inactive"
                | "not_authed"
                | "missing_scope"
                | "not_allowed_token_type"
        ) =>
        {
            (Status::Fail, "Slack rejected the token or its permissions")
        }
        _ => (Status::Warn, "Slack could not confirm the token"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn reports_slack_acceptance_rejection_and_inconclusive_responses() {
        for (body, expected) in [
            (r#"{"ok":true,"url":"wss://private-url"}"#, Status::Pass),
            (r#"{"ok":false,"error":"token_revoked"}"#, Status::Fail),
            (r#"{"ok":false,"error":"ratelimited"}"#, Status::Warn),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/api/auth.test", listener.local_addr().unwrap());
            let server = async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(&mut stream);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                        .await
                        .unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    headers.push_str(&line);
                }
                assert!(headers.starts_with("POST /api/auth.test "));
                assert!(headers
                    .to_lowercase()
                    .contains("authorization: bearer xoxp-synthetic"));
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
            };
            let ((), (status, detail)) = tokio::join!(
                server,
                verify("xoxp-synthetic", &url, Duration::from_secs(2))
            );
            assert_eq!(status, expected);
            assert!(!detail.contains("private-url"));
        }
    }
}
