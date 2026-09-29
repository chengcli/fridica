//! Bounded authenticated file reads with replayable cache decisions. The cache
//! key includes HTML eligibility and whether scopes are known, so cached bytes
//! cannot bypass a later sign-in-page check.
use super::{scrub_bytes, WebClient};
use crate::{
    core::delivery::AdapterFuture,
    slack::{
        files::{Download, Downloader, Failure, FILE_LIMIT},
        ingress::file_url,
    },
};
use reqwest::{header::AUTHORIZATION, Url};
use rusqlite::params;
use serde_json::json;
use std::{collections::VecDeque, time::Duration};
#[derive(Default)]
pub(super) struct Cache {
    success: VecDeque<((String, bool, bool), Download)>,
    failure: VecDeque<((String, bool, bool), f64, Failure)>,
}
impl Cache {
    fn get(&mut self, key: &(String, bool, bool), now: f64) -> Option<Result<Download, Failure>> {
        self.failure.retain(|(_, until, _)| *until > now);
        self.success
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| Ok(v.clone()))
            .or_else(|| {
                self.failure
                    .iter()
                    .find(|(k, _, _)| k == key)
                    .map(|(_, _, e)| Err(e.clone()))
            })
    }
    fn put(&mut self, key: (String, bool, bool), result: &Result<Download, Failure>, now: f64) {
        self.success.retain(|(k, _)| k != &key);
        self.failure
            .retain(|(k, until, _)| k != &key && *until > now);
        match result {
            Ok(value) => {
                if self.success.len() >= 32 {
                    self.success.pop_front();
                }
                self.success.push_back((key, value.clone()));
            }
            Err(failure) => {
                if self.failure.len() >= 256 {
                    self.failure.pop_front();
                }
                let ttl = match failure {
                    Failure::RateLimited { retry_after } if retry_after.is_finite() => {
                        retry_after.max(300.)
                    }
                    _ => 300.,
                };
                self.failure.push_back((key, now + ttl, failure.clone()));
            }
        }
    }
}
impl WebClient {
    fn file_target(&self, value: &str) -> Result<Url, Failure> {
        let url = Url::parse(value).map_err(|_| Failure::Url)?;
        #[cfg(test)]
        if self
            .file_origin
            .as_ref()
            .is_some_and(|base| base.origin() == url.origin())
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
        {
            return Ok(url);
        }
        if value.len() > 8192
            || !file_url(value)
            || value.contains('\\')
            || url.fragment().is_some()
        {
            return Err(Failure::Url);
        }
        Ok(url)
    }
    async fn file_bytes(
        &self,
        url: Url,
        html: bool,
        known_scopes: bool,
    ) -> Result<Download, Failure> {
        let failure = |e: reqwest::Error| {
            if e.is_timeout() {
                Failure::Timeout
            } else {
                Failure::Connection
            }
        };
        let mut response = self
            .client
            .get(url)
            .header(AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(failure)?;
        if response.status().as_u16() == 429 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|n| n.is_finite())
                .unwrap_or(30.);
            return Err(Failure::RateLimited { retry_after });
        }
        let is_html = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .eq_ignore_ascii_case("text/html");
        if response.status().as_u16() != 200 || (is_html && !html) {
            return Err(Failure::Unavailable);
        }
        if is_html && !known_scopes {
            return Err(Failure::UnknownHtml);
        }
        let size = response.content_length().unwrap_or(0);
        let mut data = Vec::new();
        while data.len() <= FILE_LIMIT {
            let Some(chunk) = response.chunk().await.map_err(failure)? else {
                break;
            };
            let n = chunk.len().min(FILE_LIMIT + 1 - data.len());
            data.extend_from_slice(&chunk[..n]);
        }
        let token = self
            .authorization
            .to_str()
            .unwrap_or("")
            .strip_prefix("Bearer ")
            .unwrap_or("");
        scrub_bytes(&mut data, token.as_bytes());
        // Redaction can expand tiny synthetic tokens; retain the same byte bound.
        data.truncate(FILE_LIMIT + 1);
        Ok(Download { data, size })
    }
    async fn download_file(&self, url: String, html: bool) -> Result<Download, Failure> {
        if !self.is_validated() {
            return Err(Failure::NotValidated);
        }
        let scopes = self
            .file_scopes
            .read()
            .map_err(|_| Failure::Recording)?
            .clone();
        if scopes.as_ref().is_some_and(|s| !s.contains("files:read")) {
            return Err(Failure::MissingScope);
        }
        let target = self.file_target(&url)?;
        let now = self.clock.now();
        if !now.is_finite() {
            return Err(Failure::InvalidResponse);
        }
        let token = self
            .authorization
            .to_str()
            .unwrap_or("")
            .strip_prefix("Bearer ")
            .unwrap_or("");
        let record =
            json!({"url":url.replace(token,"[credential]"),"html":html,"limit":FILE_LIMIT});
        let call=self.store.call(move|c|{c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('slack_file_call',?,?,0)",params![now,record.to_string()])?;Ok(c.last_insert_rowid())}).await.map_err(|_|Failure::Recording)?;
        let key = (url, html, scopes.is_some());
        let cached = self.downloads.lock().await.get(&key, now);
        let cache_hit = cached.is_some();
        let result = match cached {
            Some(result) => result,
            None => tokio::time::timeout(
                Duration::from_secs(20),
                self.file_bytes(target, html, scopes.is_some()),
            )
            .await
            .unwrap_or(Err(Failure::Timeout)),
        };
        let complete = !matches!(result, Err(Failure::Timeout | Failure::Connection));
        let record = json!({"call":call,"cache_hit":cache_hit,"result":result});
        let now = self.clock.now();
        self.store.call(move|c|{let tx=c.transaction()?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('slack_file_result',?,?,?)",params![now,record.to_string(),complete])?;
            tx.execute("UPDATE replay_events SET complete=? WHERE seq=?",params![complete,call])?;
            tx.commit()?;Ok(())
        }).await.map_err(|_|Failure::Recording)?;
        if !cache_hit {
            self.downloads.lock().await.put(key, &result, now);
        }
        result
    }
}
impl Downloader for WebClient {
    fn download(&self, url: String, html: bool) -> AdapterFuture<'_, Result<Download, Failure>> {
        Box::pin(self.download_file(url, html))
    }
}
