use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub trait Clock: Send + Sync {
    fn now(&self) -> f64;
}
pub trait Identifiers: Send + Sync {
    fn next(&self, namespace: &str) -> String;
}

pub struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
    }
}
pub struct RandomIds;
impl Identifiers for RandomIds {
    fn next(&self, namespace: &str) -> String {
        format!("{namespace}-{}", uuid::Uuid::new_v4())
    }
}
pub struct ReplayClock(AtomicU64);
impl ReplayClock {
    pub fn new(now: f64) -> Self {
        Self(AtomicU64::new(now.to_bits()))
    }
    pub fn set(&self, now: f64) {
        self.0.store(now.to_bits(), Ordering::SeqCst);
    }
}
impl Clock for ReplayClock {
    fn now(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::SeqCst))
    }
}
#[derive(Default)]
pub struct SequenceIds(AtomicU64);
impl Identifiers for SequenceIds {
    fn next(&self, namespace: &str) -> String {
        format!(
            "{namespace}-{:016x}",
            self.0.fetch_add(1, Ordering::SeqCst) + 1
        )
    }
}
