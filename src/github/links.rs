//! Bounded GitHub link reads with per-item failures, caching and rate-limit gates.
use super::{
    client::{Api, Failure, Operation, Request},
    view::{self, Aux, Pages},
};
use crate::{
    core::{delivery::AdapterFuture, time::Clock},
    store::Store,
};
use fridica_core::store::Store as _;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::sync::Mutex;

static LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://(?:www\.)?github\.com/([A-Za-z0-9][A-Za-z0-9-]{0,38})/([A-Za-z0-9._-]{1,100})/(pull|issues)/([0-9]{1,9})").unwrap()
});
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Link {
    pub owner: String,
    pub repo: String,
    pub kind: String,
    pub number: u64,
}
impl Link {
    pub fn repository(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
    pub fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/{}",
            self.repository(),
            if self.kind == "pull" {
                "pull"
            } else {
                "issues"
            },
            self.number
        )
    }
    fn key(&self) -> String {
        format!("{}#{}", self.repository().to_ascii_lowercase(), self.number)
    }
    fn request(&self, operation: Operation) -> Request {
        Request {
            repo: self.repository(),
            operation,
        }
    }
}
pub fn links(texts: &[String]) -> Vec<Link> {
    let mut found: Vec<Link> = vec![];
    for text in texts {
        for m in LINK.captures_iter(text) {
            let end = m.get(4).unwrap().end();
            if text[end..].chars().next().is_some_and(|c| c.is_numeric())
                || matches!(&m[2], "." | "..")
            {
                continue;
            }
            let number = m[4].parse::<u64>().unwrap();
            if number == 0 {
                continue;
            }
            let entry = Link {
                owner: m[1].into(),
                repo: m[2].into(),
                kind: if &m[3] == "pull" { "pull" } else { "issue" }.into(),
                number,
            };
            if found.iter().any(|v| v.key() == entry.key()) {
                continue;
            }
            found.push(entry);
            if found.len() == 3 {
                return found;
            }
        }
    }
    found
}
pub trait Reader: Send + Sync {
    fn linked(&self, texts: Vec<String>) -> AdapterFuture<'_, Result<Vec<Value>, Failure>>;
}
struct Cache {
    items: VecDeque<(String, f64, Value)>,
    paused_until: f64,
}
pub struct Links<A: Api + ?Sized> {
    api: Arc<A>,
    store: Store,
    clock: Arc<dyn Clock>,
    cache_seconds: f64,
    retry_delay: Duration,
    cache: Mutex<Cache>,
}
impl<A: Api + ?Sized> Links<A> {
    pub fn new(
        api: Arc<A>,
        store: Store,
        clock: Arc<dyn Clock>,
        cache_seconds: f64,
    ) -> Result<Self, Failure> {
        if !cache_seconds.is_finite() || cache_seconds < 0. {
            return Err(Failure::Invalid);
        }
        Ok(Self {
            api,
            store,
            clock,
            cache_seconds,
            retry_delay: Duration::from_secs(1),
            cache: Mutex::new(Cache {
                items: VecDeque::new(),
                paused_until: 0.,
            }),
        })
    }
    async fn get(&self, request: Request) -> Result<Value, Failure> {
        if self.cache.lock().await.paused_until > self.clock.now() {
            return Err(Failure::RateLimited { after: 1. });
        }
        let result = self.api.get(request).await;
        if let Err(Failure::RateLimited { after }) = &result {
            let until = self.clock.now() + after.clamp(1., 3600.);
            let mut cache = self.cache.lock().await;
            cache.paused_until = cache.paused_until.max(until);
        }
        result
    }
    async fn state(&self, link: Link) -> Result<Value, Failure> {
        let now = self.clock.now();
        if !now.is_finite() {
            return Err(Failure::Invalid);
        }
        let key = link.key();
        let cached = {
            let cache = self.cache.lock().await;
            cache
                .items
                .iter()
                .find(|(k, expiry, _)| *k == key && *expiry > now)
                .map(|(_, expiry, v)| (expiry - self.cache_seconds, v.clone()))
        };
        if let Some((fetched, mut value)) = cached {
            // A cached state is quoted as of when it was read, never as live.
            if value.is_object() {
                value["age_seconds"] = json!((now - fetched).max(0.).round());
            }
            let record = json!({"key":key,"value":value});
            self.store
                .transact(move |u| {
                    u.record("github_cache_hit", now, &record.to_string(), true)?;
                    Ok(())
                })
                .await
                .map_err(|_| Failure::Recording)?;
            return Ok(value);
        }
        let result = self.fetch(&link).await;
        if matches!(result, Err(Failure::Recording)) {
            return Err(Failure::Recording);
        }
        let value = result.unwrap_or_else(|e| json!({"link":link.url(),"error":e.message()}));
        let expiry = self.clock.now() + self.cache_seconds;
        let mut cache = self.cache.lock().await;
        cache
            .items
            .retain(|(k, t, _)| *k != key && *t > self.clock.now());
        while cache.items.len() >= 256 {
            cache.items.pop_front();
        }
        cache.items.push_back((key, expiry, value.clone()));
        let mut value = value;
        if value.is_object() {
            value["age_seconds"] = json!(0.);
        }
        Ok(value)
    }
    async fn object(&self, link: &Link, operation: Operation) -> Result<Value, Failure> {
        let value = self.get(link.request(operation)).await?;
        if !value.is_object() || value["number"].as_u64() != Some(link.number) {
            return Err(Failure::Invalid);
        }
        Ok(value)
    }
    async fn fetch(&self, link: &Link) -> Result<Value, Failure> {
        if link.kind == "issue" {
            let issue = self
                .object(
                    link,
                    Operation::Issue {
                        number: link.number,
                    },
                )
                .await?;
            if issue
                .get("pull_request")
                .is_none_or(|v| v.is_null() || v == false)
            {
                return Ok(view::issue(&link.repository(), &issue));
            }
        }
        let mut pull = match self
            .object(
                link,
                Operation::Pull {
                    number: link.number,
                },
            )
            .await
        {
            Err(Failure::NotFound) if link.kind == "pull" => {
                return Ok(view::issue(
                    &link.repository(),
                    &self
                        .object(
                            link,
                            Operation::Issue {
                                number: link.number,
                            },
                        )
                        .await?,
                ))
            }
            other => other?,
        };
        if pull["state"] == "open"
            && (pull["mergeable_state"].is_null() || pull["mergeable_state"] == "unknown")
        {
            tokio::time::sleep(self.retry_delay).await;
            pull = self
                .object(
                    link,
                    Operation::Pull {
                        number: link.number,
                    },
                )
                .await?;
        }
        let head = pull["head"]["sha"]
            .as_str()
            .filter(|s| super::client::sha(s))
            .ok_or(Failure::Invalid)?
            .to_owned();
        let base = pull["base"]["ref"]
            .as_str()
            .ok_or(Failure::Invalid)?
            .to_owned();
        let tree = self.get(link.request(Operation::Tree { head: head.clone() }));
        let behind = self.get(link.request(Operation::Compare {
            head: head.clone(),
            base,
        }));
        // Propagate recording faults immediately. Waiting for a hung sibling
        // could let the parent's optional-read timeout conceal the audit failure.
        let (tree, behind, runs, reviews) = tokio::try_join!(
            async { optional(tree.await) },
            async { optional(behind.await) },
            async { optional(self.pages(link, &head, true).await) },
            async { optional(self.pages(link, &head, false).await) },
        )?;
        let tree = tree.and_then(|v| {
            v["tree"]["sha"]
                .as_str()
                .filter(|s| super::client::sha(s))
                .map(str::to_owned)
        });
        let behind = behind.and_then(|v| v["behind_by"].as_i64());
        Ok(view::pull(
            &link.repository(),
            &pull,
            Aux {
                tree,
                behind,
                runs,
                reviews,
            },
        ))
    }
    async fn pages(&self, link: &Link, head: &str, checks: bool) -> Result<Pages, Failure> {
        let mut items = vec![];
        for page in 1..=3 {
            let operation = if checks {
                Operation::Checks {
                    head: head.into(),
                    page,
                }
            } else {
                Operation::Reviews {
                    number: link.number,
                    page,
                }
            };
            let value = self.get(link.request(operation)).await?;
            let batch = if checks {
                value["check_runs"].as_array()
            } else {
                value.as_array()
            }
            .ok_or(Failure::Invalid)?;
            // Never certify omitted entries as success, even for a broken server.
            if batch.len() > 100 {
                items.extend(batch.iter().take(100).cloned());
                return Ok((items, false));
            }
            items.extend(batch.iter().cloned());
            if checks {
                let total = value["total_count"].as_u64().unwrap_or(items.len() as u64);
                if items.len() as u64 >= total || batch.len() < 100 {
                    return Ok((items, items_len_complete(total, batch.len(), page)));
                }
            } else if batch.len() < 100 {
                return Ok((items, true));
            }
        }
        Ok((items, false))
    }
}
fn items_len_complete(total: u64, last: usize, page: usize) -> bool {
    ((page - 1) * 100 + last) as u64 >= total
}
fn optional<T>(value: Result<T, Failure>) -> Result<Option<T>, Failure> {
    match value {
        Err(Failure::Recording) => Err(Failure::Recording),
        other => Ok(other.ok()),
    }
}
impl<A: Api + ?Sized> Reader for Links<A> {
    fn linked(&self, texts: Vec<String>) -> AdapterFuture<'_, Result<Vec<Value>, Failure>> {
        Box::pin(async move {
            futures_util::future::try_join_all(
                links(&texts).into_iter().map(|link| self.state(link)),
            )
            .await
        })
    }
}
