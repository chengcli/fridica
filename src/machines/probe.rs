//! Lightweight machine load readings for placement. The probe runs one fixed,
//! read-only script; agents never influence it. Readings are data: they are
//! recorded with the parent request, so placement stays a pure function of it.
use crate::{
    config::{registry::Machine, schema::Placement, Config},
    core::{delivery::AdapterFuture, time::Clock},
    store::Store,
};
use anyhow::Result;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

/// Fixed probe: 1-minute load average and online CPUs (Linux /proc or macOS
/// sysctl), plus per-GPU utilization and memory where `nvidia-smi` exists.
pub const SCRIPT: &str = r#"export LC_ALL=C
if [ -r /proc/loadavg ]; then read a _ < /proc/loadavg; else a=$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | awk '{print $1}'); fi
n=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null)
echo "load ${a:-} ${n:-}"
if command -v nvidia-smi >/dev/null 2>&1; then
  nvidia-smi --query-gpu=index,utilization.gpu,memory.used,memory.total --format=csv,noheader,nounits 2>/dev/null | head -n 64 | sed 's/^/gpu /'
fi
"#;
/// Probe output beyond this is ignored as malformed.
pub const OUTPUT_LIMIT: usize = 16 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Gpu {
    pub index: usize,
    pub utilization: f64,
    pub memory_used_mb: f64,
    pub memory_total_mb: f64,
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Reading {
    pub load1: f64,
    pub cpus: usize,
    pub gpus: Vec<Gpu>,
    /// Probe time; readings older than the configured TTL are refreshed.
    pub at: f64,
}
/// Placement view of one machine: recorded in the request next to `busy`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Assessment {
    /// Highest of the CPU load ratio and the fraction of declared GPUs in use.
    pub score: f64,
    pub saturated: bool,
    pub reading: Reading,
}
fn number(text: &str) -> Option<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.)
}
/// Parse probe output; `None` when the load line is missing or malformed.
pub fn parse(output: &[u8], at: f64) -> Option<Reading> {
    if output.len() > OUTPUT_LIMIT {
        return None;
    }
    let text = std::str::from_utf8(output).ok()?;
    let mut reading = None;
    let mut gpus = vec![];
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("load ") {
            let mut fields = rest.split_whitespace();
            let load1 = number(fields.next()?)?;
            let cpus = fields.next()?.parse::<usize>().ok().filter(|n| *n > 0)?;
            reading = Some((load1, cpus));
        } else if let Some(rest) = line.strip_prefix("gpu ") {
            let fields: Vec<_> = rest.split(',').collect();
            if let [index, utilization, used, total] = fields[..] {
                if let (Ok(index), Some(utilization), Some(used), Some(total)) = (
                    index.trim().parse::<usize>(),
                    number(utilization),
                    number(used),
                    number(total),
                ) {
                    gpus.push(Gpu {
                        index,
                        utilization,
                        memory_used_mb: used,
                        memory_total_mb: total,
                    });
                }
            }
        }
    }
    let (load1, cpus) = reading?;
    Some(Reading {
        load1,
        cpus,
        gpus,
        at,
    })
}
fn gpu_in_use(gpu: &Gpu, thresholds: &Placement) -> bool {
    gpu.utilization >= thresholds.max_gpu_utilization
        || (gpu.memory_total_mb > 0.
            && gpu.memory_used_mb / gpu.memory_total_mb >= thresholds.max_gpu_memory)
}
/// Judge a reading against the machine's declared resources. Declared CPUs and
/// GPUs bound what Fridica may use; GPUs the probe cannot see are not counted.
pub fn assess(machine: &Machine, reading: Reading, thresholds: &Placement) -> Assessment {
    let cpus = machine.resources.cpus.unwrap_or(reading.cpus).max(1) as f64;
    let cpu = reading.load1 / cpus;
    let cpu_saturated = cpu >= thresholds.max_load;
    let (gpu, gpu_saturated) = match &machine.resources.gpus {
        Some(declared) if !declared.is_empty() => {
            let seen: Vec<_> = reading
                .gpus
                .iter()
                .filter(|g| declared.contains(&g.index))
                .collect();
            if seen.is_empty() {
                (0., false)
            } else {
                let used = seen.iter().filter(|g| gpu_in_use(g, thresholds)).count();
                (used as f64 / seen.len() as f64, used == seen.len())
            }
        }
        _ => (0., false),
    };
    Assessment {
        score: cpu.max(gpu),
        saturated: cpu_saturated || gpu_saturated,
        reading,
    }
}

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
                    .call(move |c| {
                        c.execute(
                            "INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('machine_load_call',?,?,0)",
                            params![now, payload],
                        )?;
                        Ok(c.last_insert_rowid())
                    })
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
                .call(move |c| {
                    let tx = c.transaction()?;
                    tx.execute("UPDATE replay_events SET complete=1 WHERE seq=?", [call])?;
                    tx.execute(
                        "INSERT INTO replay_events(kind,time,payload_json) VALUES('machine_load_result',?,?)",
                        params![done, payload],
                    )?;
                    tx.commit()?;
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
    use crate::config::registry::Resources;

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
