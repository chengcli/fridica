//! No database writes, redirects, proxies or automatic retries. A lost response
//! may follow committed effects; retry instructions only with the same client ID.
use super::RESPONSE_LIMIT;
use serde_json::Value;
use std::{
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::Path,
    time::Duration,
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Unavailable,
    InvalidRequest,
    InvalidResponse,
    UnsafeSocket,
    Rejected { status: u16, code: String },
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "control request failed: {self:?}")
    }
}
impl std::error::Error for Failure {}
pub struct Client {
    client: reqwest::Client,
    token: Option<String>,
}
impl Client {
    pub fn new(socket: &Path, token: Option<String>) -> Result<Self, Failure> {
        let m = std::fs::symlink_metadata(socket).map_err(|_| Failure::Unavailable)?;
        if !m.file_type().is_socket()
            || m.uid() != users::get_current_uid()
            || m.mode() & 0o077 != 0
        {
            return Err(Failure::UnsafeSocket);
        }
        if token
            .as_ref()
            .is_some_and(|t| t.len() != 64 || !t.bytes().all(|c| c.is_ascii_hexdigit()))
        {
            return Err(Failure::InvalidRequest);
        }
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| Failure::Unavailable)?;
        Ok(Self { client, token })
    }
    pub async fn request(
        &self,
        method: &str,
        target: &str,
        body: Option<Value>,
    ) -> Result<Value, Failure> {
        if super::api::target(target).is_none() || !matches!(method, "GET" | "POST" | "PATCH") {
            return Err(Failure::InvalidRequest);
        }
        let mut request = self.client.request(
            reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| Failure::InvalidRequest)?,
            format!("http://fridica{target}"),
        );
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            if !body.is_object()
                || serde_json::to_vec(&body)
                    .map_err(|_| Failure::InvalidRequest)?
                    .len()
                    > super::BODY_LIMIT
            {
                return Err(Failure::InvalidRequest);
            }
            request = request.json(&body);
        }
        let mut response = request.send().await.map_err(|_| Failure::Unavailable)?;
        let status = response.status().as_u16();
        let mut bytes = vec![];
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::InvalidResponse)?
        {
            if chunk.len() > RESPONSE_LIMIT.saturating_sub(bytes.len()) {
                return Err(Failure::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::InvalidResponse)?;
        if status >= 400 {
            let code = value["error"]
                .as_str()
                .filter(|v| v.len() <= 80 && v.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'))
                .unwrap_or("control_error")
                .to_owned();
            return Err(Failure::Rejected { status, code });
        }
        if status != 200 {
            return Err(Failure::InvalidResponse);
        }
        Ok(value)
    }
}
