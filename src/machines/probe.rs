//! Lightweight machine load readings for placement. The probe runs one fixed,
//! read-only script; agents never influence it. Readings are data: they are
//! recorded with the parent request, so placement stays a pure function of it.
use crate::{
    config::{registry::Machine, Config},
    core::{delivery::AdapterFuture, time::Clock},
    store::Store,
};
use anyhow::Result;
use fridica_core::store::Store as _;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

pub use fridica_core::placement::probe::{
    assess, parse, Assessment, Gpu, Reading, OUTPUT_LIMIT, SCRIPT,
};

/// Runs the fixed probe on one machine; `None` when it fails or times out.
pub trait Reader: Send + Sync {
    fn read<'a>(
        &'a self,
        machine: &'a Machine,
        timeout: Duration,
    ) -> AdapterFuture<'a, Option<Reading>>;
}
/// Caches one reading per machine for `probe_ttl`, including failures, so an
/// unreachable host costs at most one timeout per TTL rather than one per turn.
pub struct Monitor {
    reader: Arc<dyn Reader>,
    cache: tokio::sync::Mutex<BTreeMap<String, (f64, Option<Reading>)>>,
}
impl Monitor {
    pub fn new(reader: Arc<dyn Reader>) -> Self {
        Self {
            reader,
            cache: Default::default(),
        }
    }
    /// Assess every configured machine, probing stale ones in parallel. Each probe
    /// is a recorded replay boundary; machines without a reading are omitted.
    pub async fn assess(
        &self,
        config: &Config,
        store: &Store,
        clock: &dyn Clock,
    ) -> Result<BTreeMap<String, Assessment>> {
        let placement = &config.placement;
        let now = clock.now();
        let mut cache = self.cache.lock().await;
        let stale: Vec<&Machine> = config
            .machines
            .machines
            .iter()
            .filter(|m| {
                cache
                    .get(&m.name)
                    .is_none_or(|(at, _)| now - at >= placement.probe_ttl)
            })
            .collect();
        let mut calls = Vec::with_capacity(stale.len());
        for machine in &stale {
            let payload = json!({"machine": machine.name}).to_string();
            calls.push(
                store
                    .transact(move |u| u.record("machine_load_call", now, &payload, false))
                    .await?,
            );
        }
        let timeout = Duration::from_secs_f64(placement.probe_timeout);
        let readings = futures_util::future::join_all(stale.iter().map(|m| async move {
            tokio::time::timeout(timeout, self.reader.read(m, timeout))
                .await
                .ok()
                .flatten()
        }))
        .await;
        let done = clock.now();
        for ((machine, call), reading) in stale.iter().zip(calls).zip(readings) {
            let reading = reading.map(|mut r| {
                r.at = done;
                r
            });
            let payload =
                json!({"call": call, "machine": machine.name, "reading": reading}).to_string();
            store
                .transact(move |u| {
                    u.complete(call, true)?;
                    u.record("machine_load_result", done, &payload, true)?;
                    Ok(())
                })
                .await?;
            cache.insert(machine.name.clone(), (done, reading));
        }
        Ok(config
            .machines
            .machines
            .iter()
            .filter_map(|m| {
                let reading = cache.get(&m.name)?.1.clone()?;
                Some((m.name.clone(), assess(m, reading, placement)))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{registry::Resources, schema::Placement};

    fn machine(cpus: Option<usize>, gpus: Option<Vec<usize>>) -> Machine {
        Machine {
            name: "m".into(),
            transport: "local".into(),
            workspaces: vec![],
            backends: vec!["claude".into()],
            default_backend: "claude".into(),
            policy: Default::default(),
            host: String::new(),
            tags: vec![],
            resources: Resources {
                cpus,
                gpus,
                ..Default::default()
            },
            max_workers: 4,
            max_jobs: 2,
            slurm: None,
            description: String::new(),
        }
    }

    struct Scripted(std::sync::Mutex<Vec<String>>);
    impl Reader for Scripted {
        fn read<'a>(
            &'a self,
            machine: &'a Machine,
            _: Duration,
        ) -> AdapterFuture<'a, Option<Reading>> {
            self.0.lock().unwrap().push(machine.name.clone());
            Box::pin(async move {
                match machine.name.as_str() {
                    "up" => parse(b"load 12 64\n", 0.),
                    "hung" => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        None
                    }
                    _ => None,
                }
            })
        }
    }

    #[tokio::test]
    async fn monitor_records_each_probe_and_caches_readings_and_failures_for_the_ttl() {
        use crate::core::time::ReplayClock;
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
[owner]
slack_user = "UOWNER"
[slack]
workspace = "TTEAM"
channels = ["CROOM"]
[placement]
probe_ttl = 60
probe_timeout = 1
[machines.up]
host = "up"
resources = {cpus = 24}
[machines.up.workspaces]
w = "/work/up"
[machines.down]
host = "down"
[machines.down.workspaces]
w = "/work/down"
[machines.hung]
host = "hung"
[machines.hung.workspaces]
w = "/work/hung"
"#;
        let context = crate::config::LoadContext {
            home: dir.path().into(),
            runtime_dir: None,
            uid: 1,
            protected: vec![],
        };
        let config =
            crate::config::loader::parse(source, &dir.path().join("config.toml"), &context)
                .unwrap();
        let store = Store::open(dir.path().join("db")).await.unwrap();
        let clock = ReplayClock::new(1000.);
        let reader = Arc::new(Scripted(Default::default()));
        let monitor = Monitor::new(reader.clone());
        let started = std::time::Instant::now();
        let first = monitor.assess(&config, &store, &clock).await.unwrap();
        // Parallel probes: a hung host costs one timeout, not one per machine.
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(first.keys().collect::<Vec<_>>(), ["up"]);
        assert_eq!((first["up"].score, first["up"].saturated), (0.5, false));
        let events: Vec<(String, i64)> = store
            .call(|c| {
                Ok(
                    c.prepare("SELECT kind, complete FROM replay_events ORDER BY seq")?
                        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<rusqlite::Result<_>>()?,
                )
            })
            .await
            .unwrap();
        let count = |kind: &str| {
            events
                .iter()
                .filter(|(k, done)| k == kind && *done == 1)
                .count()
        };
        assert_eq!(
            (
                events.len(),
                count("machine_load_call"),
                count("machine_load_result")
            ),
            (6, 3, 3)
        );
        // Within the TTL neither readings nor failures are probed again.
        clock.set(1030.);
        assert_eq!(
            monitor.assess(&config, &store, &clock).await.unwrap(),
            first
        );
        assert_eq!(reader.0.lock().unwrap().len(), 3);
        clock.set(1061.);
        monitor.assess(&config, &store, &clock).await.unwrap();
        assert_eq!(reader.0.lock().unwrap().len(), 6);
    }

    #[test]
    fn parses_linux_macos_and_gpu_lines_and_rejects_malformed_output() {
        let text = b"load 3.50 8\ngpu 0, 97, 30000, 32607\ngpu 1, 0, 3, 32607\ngpu x, 1, 2, 3\n";
        let reading = parse(text, 10.).unwrap();
        assert_eq!((reading.load1, reading.cpus, reading.at), (3.5, 8, 10.));
        assert_eq!(reading.gpus.len(), 2);
        assert_eq!(reading.gpus[0].memory_used_mb, 30000.);
        assert_eq!(parse(b"load 1.2 10\n", 0.).unwrap().gpus, vec![]);
        for bad in [
            &b""[..],
            b"load \n",
            b"load 1 0\n",
            b"load -1 4\n",
            b"load NaN 4\n",
            b"gpu 0, 1, 2, 3\n",
        ] {
            assert!(parse(bad, 0.).is_none(), "{bad:?}");
        }
        assert!(parse(&vec![b'x'; OUTPUT_LIMIT + 1], 0.).is_none());
    }

    #[test]
    fn declared_resources_bound_cpu_and_gpu_saturation() {
        let t = Placement::default();
        let read = |text: &[u8]| parse(text, 0.).unwrap();
        // 6 of 24 declared CPUs busy, no GPUs declared.
        let a = assess(&machine(Some(24), None), read(b"load 6 64\n"), &t);
        assert_eq!((a.score, a.saturated), (0.25, false));
        // Load counted against declared CPUs, not the host's.
        assert!(assess(&machine(Some(4), None), read(b"load 4 64\n"), &t).saturated);
        // One of two declared GPUs busy by utilization, the other by memory.
        let gpus = b"load 1 24\ngpu 0, 95, 100, 1000\ngpu 1, 5, 950, 1000\ngpu 2, 0, 0, 1000\n";
        let a = assess(&machine(Some(24), Some(vec![0, 2])), read(gpus), &t);
        assert_eq!((a.score, a.saturated), (0.5, false));
        assert!(assess(&machine(Some(24), Some(vec![0, 1])), read(gpus), &t).saturated);
        // Declared GPUs the probe cannot see are unknown, not saturated.
        let a = assess(&machine(Some(24), Some(vec![5])), read(gpus), &t);
        assert_eq!((a.score, a.saturated), (1. / 24., false));
    }
}
