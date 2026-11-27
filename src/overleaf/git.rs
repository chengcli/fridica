/// Verify read access to the default Overleaf project using Git's read-only
/// reference advertisement. A project ID is needed to authenticate the token.
pub async fn probe_overleaf() -> anyhow::Result<bool> {
    fridica_agent::overleaf::probe().await?;
    let Some(project) = std::env::var("OVERLEAF_PROJECT_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
    else {
        return Ok(false);
    };
    anyhow::ensure!(
        project.len() == 24 && project.bytes().all(|c| c.is_ascii_hexdigit()),
        "OVERLEAF_PROJECT_ID must be a 24-digit hexadecimal project ID"
    );
    let url = format!("https://git.overleaf.com/{project}/info/refs?service=git-upload-pack");
    verify_git_access(&url, &std::env::var("OVERLEAF_GIT_TOKEN")?).await?;
    Ok(true)
}

async fn verify_git_access(url: &str, token: &str) -> anyhow::Result<()> {
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()?
        .get(url)
        .basic_auth("git", Some(token))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Overleaf project check failed"))?;
    anyhow::ensure!(
        response.status().is_success()
            && response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(
                    |v| v.split(';').next() == Some("application/x-git-upload-pack-advertisement")
                ),
        "Overleaf project read access could not be verified"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn overleaf_verifies_an_authenticated_git_read() {
        crate::exec::tls::initialize();
        for (status, content_type, expected) in [
            (
                "200 OK",
                "application/x-git-upload-pack-advertisement",
                true,
            ),
            ("200 OK", "text/html", false),
            ("401 Unauthorized", "text/plain", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!(
                "http://{}/project/info/refs?service=git-upload-pack",
                listener.local_addr().unwrap()
            );
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(
                    line,
                    "GET /project/info/refs?service=git-upload-pack HTTP/1.1\r\n"
                );
                let mut authenticated = false;
                loop {
                    line.clear();
                    if reader.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    authenticated |=
                        line.eq_ignore_ascii_case("authorization: Basic Z2l0OnRlc3Q=\r\n");
                }
                assert!(authenticated);
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: 0\r\n\r\n");
                reader
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            });
            assert_eq!(verify_git_access(&url, "test").await.is_ok(), expected);
            server.await.unwrap();
        }
    }
}
