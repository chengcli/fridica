//! `fridica dashboard`: the embedded control-room page, served on loopback.
//!
//! A small HTTP server on `127.0.0.1` serves the page's four files and
//! forwards `/api/<route>` to the daemon's owner-only control socket. Each
//! run makes a fresh random key; the printed URL carries it in its fragment
//! (`#key=…`, never sent to a server or kept in history logs), and every API
//! call must present it as a bearer token. The `Host` header must name this
//! loopback address, so a page on another origin cannot reach the server
//! through DNS rebinding; nothing sets CORS headers.
use crate::control::{client::Client, client::Failure, BODY_LIMIT};
use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    body::{Bytes, Incoming},
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{convert::Infallible, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::net::TcpListener;

/// The page and its assets, as embedded in this build.
const FILES: [(&str, &str, &[u8]); 4] = [
    (
        "/",
        "text/html; charset=utf-8",
        include_bytes!("../../assets/dashboard/index.html"),
    ),
    (
        "/app.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../../assets/dashboard/app.js"),
    ),
    (
        "/app.css",
        "text/css; charset=utf-8",
        include_bytes!("../../assets/dashboard/app.css"),
    ),
    (
        "/fridica-logo.png",
        "image/png",
        include_bytes!("../../assets/dashboard/fridica-logo.png"),
    ),
];
const SECURITY: [(&str, &str); 5] = [
    (
        "content-security-policy",
        "default-src 'self'; img-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("cache-control", "no-store"),
    ("x-frame-options", "DENY"),
];

/// A fresh 256-bit key for one run, as 64 hex characters.
pub fn key() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

struct Server {
    socket: PathBuf,
    port: u16,
    /// Only the key's digest is kept, and compared in full.
    digest: [u8; 32],
}
impl Server {
    fn authorized(&self, request: &hyper::Request<Incoming>) -> bool {
        let presented = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        let digest: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        // Equal-length digests compared without an early exit.
        digest
            .iter()
            .zip(self.digest.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
    fn local_host(&self, request: &hyper::Request<Incoming>) -> bool {
        let host = request
            .headers()
            .get("host")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        host == format!("127.0.0.1:{}", self.port) || host == format!("localhost:{}", self.port)
    }
}

fn reply(status: u16, kind: &str, body: Vec<u8>) -> hyper::Response<Full<Bytes>> {
    let mut response = hyper::Response::builder()
        .status(status)
        .header("content-type", kind);
    for (name, value) in SECURITY {
        response = response.header(name, value);
    }
    response
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| hyper::Response::new(Full::new(Bytes::new())))
}
fn error(status: u16, code: &str) -> hyper::Response<Full<Bytes>> {
    reply(
        status,
        "application/json",
        json!({"error":code}).to_string().into_bytes(),
    )
}

async fn handle(
    server: Arc<Server>,
    request: hyper::Request<Incoming>,
) -> Result<hyper::Response<Full<Bytes>>, Infallible> {
    if !server.local_host(&request) {
        return Ok(error(421, "unexpected_host"));
    }
    let method = request.method().as_str().to_owned();
    let target = request
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    let Some(route) = target.strip_prefix("/api") else {
        if method != "GET" {
            return Ok(error(405, "method_not_allowed"));
        }
        let path = request.uri().path();
        let path = if path == "/index.html" { "/" } else { path };
        return Ok(match FILES.iter().find(|(name, ..)| *name == path) {
            Some((_, kind, data)) => reply(200, kind, data.to_vec()),
            None => error(404, "not_found"),
        });
    };
    if !server.authorized(&request) {
        return Ok(error(401, "locked"));
    }
    if !matches!(method.as_str(), "GET" | "POST" | "PATCH") {
        return Ok(error(405, "method_not_allowed"));
    }
    let route = route.to_owned();
    let body = match Limited::new(request.into_body(), BODY_LIMIT)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(_) => return Ok(error(413, "body_too_large")),
    };
    let body: Option<Value> = if body.is_empty() {
        None
    } else {
        match serde_json::from_slice(&body) {
            Ok(value) => Some(value),
            Err(_) => return Ok(error(400, "invalid_json")),
        }
    };
    // The dashboard runs as the owner, so the socket grants owner authority.
    let result = match Client::new(&server.socket, None) {
        Ok(client) => client.request(&method, &route, body).await,
        Err(failure) => Err(failure),
    };
    Ok(match result {
        Ok(value) => reply(
            200,
            "application/json",
            serde_json::to_vec(&value).unwrap_or_default(),
        ),
        Err(Failure::Rejected { status, code }) => error(status, &code),
        Err(Failure::InvalidRequest) => error(400, "invalid_request"),
        Err(_) => error(502, "daemon_unavailable"),
    })
}

/// Serve until `stop` resolves. `port` 0 picks a free one; the bound address
/// and the page's URL (with the key) are reported through `ready`.
pub async fn serve(
    socket: PathBuf,
    port: u16,
    key: &str,
    ready: impl FnOnce(SocketAddr),
    stop: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .context("cannot listen on 127.0.0.1")?;
    let address = listener.local_addr()?;
    let server = Arc::new(Server {
        socket,
        port: address.port(),
        digest: Sha256::digest(key.as_bytes()).into(),
    });
    ready(address);
    tokio::pin!(stop);
    loop {
        tokio::select! {
            _ = &mut stop => return Ok(()),
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                if !peer.ip().is_loopback() {
                    continue;
                }
                let server = server.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| handle(server.clone(), request));
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder
                        .timer(TokioTimer::new())
                        .header_read_timeout(Duration::from_secs(10))
                        .max_buf_size(64 * 1024);
                    let _ = builder.serve_connection(TokioIo::new(stream), service).await;
                });
            }
        }
    }
}
