use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{Result, config::Config, exchange::Observation};

#[derive(Default, Serialize)]
pub struct Counts {
    pub dispatched: u64,
    pub completed: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub skipped_capacity: u64,
    pub skipped_schedule: u64,
    pub response_bytes: u64,
    pub errors: BTreeMap<String, u64>,
    pub rcodes: BTreeMap<u16, u64>,
    #[serde(skip)]
    latency: Vec<u64>,
}

impl Counts {
    pub fn observe(&mut self, observation: &Observation) {
        self.completed += 1;
        self.response_bytes += observation.bytes as u64;
        match observation.outcome {
            Ok(rcode) => {
                self.succeeded += 1;
                *self.rcodes.entry(rcode).or_default() += 1;
                // 固定对数桶：每个 2 倍区间分成 16 桶，不保存逐请求样本。
                let micros = observation.latency_us.max(1);
                let exponent = 63 - micros.leading_zeros() as usize;
                let base = 1_u64 << exponent;
                let fraction = ((u128::from(micros - base) * 16) / u128::from(base)) as usize;
                self.latency.resize(64 * 16, 0);
                self.latency[exponent * 16 + fraction] += 1;
            }
            Err(error) => {
                self.failed += 1;
                let key = match (error.os_code, error.http_status) {
                    (Some(code), _) => format!("{}:os={code}", error.class),
                    (_, Some(status)) => format!("{}:status={status}", error.class),
                    _ => error.class.to_owned(),
                };
                *self.errors.entry(key).or_default() += 1;
            }
        }
    }

    /// 返回成功请求延迟的桶上界；无成功样本时输出 null，不伪报零延迟。
    fn percentile(&self, numerator: u64) -> Option<u64> {
        if self.succeeded == 0 {
            return None;
        }
        let rank = (self.succeeded * numerator).div_ceil(100);
        let mut cumulative = 0;
        for (index, count) in self.latency.iter().enumerate() {
            cumulative += count;
            if cumulative >= rank {
                let base = 1_u128 << (index / 16);
                let upper = base + (base * (index as u128 % 16 + 1)).div_ceil(16);
                return Some(upper.min(u128::from(u64::MAX)) as u64);
            }
        }
        None
    }

    pub fn snapshot(&self) -> serde_json::Value {
        json!({
            "counts": self,
            "success_latency_us_upper_bound": {
                "p50": self.percentile(50),
                "p95": self.percentile(95),
                "p99": self.percentile(99)
            }
        })
    }
}

pub struct Reporter {
    pub directory: PathBuf,
    file: File,
}

impl Reporter {
    pub fn create(config: &Config, input: &[u8], root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let directory = root.join(format!("load-{}-{}", utc_ms()?, std::process::id()));
        fs::create_dir(&directory)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join("samples.jsonl"))?;
        let mut reporter = Self { directory, file };
        reporter.write(&json!({
            "event": "started",
            "utc_ms": utc_ms()?,
            "os": std::env::consts::OS,
            "architecture": std::env::consts::ARCH,
            "driver_version": env!("CARGO_PKG_VERSION"),
            "debug_assertions": cfg!(debug_assertions),
            "driver_sha256": file_sha256(&std::env::current_exe()?)?,
            "compiled_lockfile_sha256": hash(include_bytes!("../../Cargo.lock")),
            "config_sha256": hash(input),
            "targets": config.targets.iter().map(|target| json!({
                "alias": target.alias, "protocol": target.protocol
            })).collect::<Vec<_>>(),
            "query_count": config.queries.len(),
            "phases": config.phases,
            "cycles": config.cycles,
            "concurrency": config.concurrency,
            "timeout_ms": config.timeout_ms,
            "sample_interval_ms": config.sample_interval_ms,
            "max_errors": config.max_errors,
            "max_scheduler_lag_ms": config.max_scheduler_lag_ms,
            "reuse_connections": config.reuse_connections,
            "seed": config.seed,
            "resource_sampling": "external_required",
            "acceptance": "measurement_only"
        }))?;
        Ok(reporter)
    }

    pub fn write(&mut self, value: &serde_json::Value) -> Result<()> {
        serde_json::to_writer(&mut self.file, value)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }

    pub fn finish(&mut self, value: &serde_json::Value) -> Result<()> {
        self.write(value)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.directory.join("report.json"))?;
        serde_json::to_writer_pretty(file, value)?;
        Ok(())
    }
}

pub fn utc_ms() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn file_sha256(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
