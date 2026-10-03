//! `fridica dashboard`: the page on loopback, its key, and the forwarding of
//! `/api/*` to the control socket (here a fake that echoes what it gets).
use http_body_util::{BodyExt, Full};
use hyper::{body::Bytes, service::service_fn};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{convert::Infallible, os::unix::fs::PermissionsExt};
use tokio::net::UnixListener;

async fn fake_control(path: std::path::PathBuf) {
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let service = service_fn(
                    |request: hyper::Request<hyper::body::Incoming>| async move {
                        let method = request.method().to_string();
                        let target = request.uri().path_and_query().unwrap().to_string();
                        let body = request.into_body().collect().await.unwrap().to_bytes();
                        let (status, value) = if target == "/missing" {
                            (404, json!({"error":"not_found"}))
                        } else {
                            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                            (200, json!({"method":method,"target":target,"body":body}))
                        };
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(status)
                                .body(Full::new(Bytes::from(value.to_string())))
                                .unwrap(),
                        )
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
}

#[tokio::test]
async fn the_page_is_served_and_api_calls_need_the_runs_key() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("control.sock");
    fake_control(socket.clone()).await;
    let key = fridica::dashboard::key();
    assert_eq!(key.len(), 64);
    let (bound, address) = tokio::sync::oneshot::channel();
    let server_key = key.clone();
    tokio::spawn(async move {
        fridica::dashboard::serve(
            socket,
            0,
            &server_key,
            |a| {
                let _ = bound.send(a);
            },
            std::future::pending(),
        )
        .await
        .unwrap();
    });
    let address = address.await.unwrap();
    assert!(address.ip().is_loopback());
    let base = format!("http://{address}");
    let http = reqwest::Client::builder().no_proxy().build().unwrap();

    let page = http.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert!(page.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("default-src 'self'"));
    assert!(page.text().await.unwrap().contains("app.js"));
    for asset in ["/app.js", "/app.css", "/fridica-logo.png", "/index.html"] {
        assert_eq!(
            http.get(format!("{base}{asset}"))
                .send()
                .await
                .unwrap()
                .status(),
            200,
            "{asset}"
        );
    }
    assert_eq!(
        http.get(format!("{base}/secret"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // Without the key, or with another, the API is locked.
    let locked = http.get(format!("{base}/api/status")).send().await.unwrap();
    assert_eq!(locked.status(), 401);
    assert_eq!(locked.json::<Value>().await.unwrap()["error"], "locked");
    let wrong = http
        .get(format!("{base}/api/status"))
        .bearer_auth(fridica::dashboard::key())
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    // Another host name (DNS rebinding) is refused before anything else.
    let rebound = http
        .get(format!("{base}/api/status"))
        .header("host", "evil.example:80")
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(rebound.status(), 421);

    // With the key, calls reach the control socket and come back unchanged.
    let status: Value = http
        .get(format!("{base}/api/status"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        status,
        json!({"method":"GET","target":"/status","body":null})
    );
    let posted: Value = http
        .post(format!("{base}/api/threads/T:C:1.1/instruct"))
        .bearer_auth(&key)
        .json(&json!({"text":"hi","client_id":"c1"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(posted["method"], "POST");
    assert_eq!(posted["body"], json!({"text":"hi","client_id":"c1"}));
    let patched = http
        .patch(format!("{base}/api/config/parent"))
        .bearer_auth(&key)
        .json(&json!({"model":"m"}))
        .send()
        .await
        .unwrap();
    assert_eq!(patched.status(), 200);
    let missing = http
        .get(format!("{base}/api/missing"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    assert_eq!(missing.json::<Value>().await.unwrap()["error"], "not_found");
    let deleted = http
        .delete(format!("{base}/api/status"))
        .bearer_auth(&key)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), 405);
}
