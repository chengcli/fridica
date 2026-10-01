//! Machine load readings for placement. The probe is one fixed, read-only
//! script; readings are data recorded with the parent request, so placement
//! stays a pure function of it. Running the probe belongs to the host.
use crate::config::{registry::Machine, Placement};
use serde::{Deserialize, Serialize};

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
