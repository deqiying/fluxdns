//! 内存分析契约：OS 实测、规则已知载荷与预算计费不混合求和。

use serde::{Deserialize, Serialize};

use super::{DecimalU64, Measurement};

/// 同一采样轮次的进程内存分项；两份 proc 文件不保证原子一致。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryOsSample {
    pub sampled_at_ms: u64,
    pub rss_bytes: Measurement<DecimalU64>,
    pub peak_rss_bytes: Measurement<DecimalU64>,
    pub anonymous_bytes: Measurement<DecimalU64>,
    pub file_bytes: Measurement<DecimalU64>,
    pub shared_bytes: Measurement<DecimalU64>,
    pub pss_bytes: Measurement<DecimalU64>,
    pub private_dirty_bytes: Measurement<DecimalU64>,
    pub swap_bytes: Measurement<DecimalU64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryModuleId {
    Rules,
    DnsCache,
    DetailsWs,
    StatisticsQueues,
    Sqlite,
    Allocator,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryProbeState {
    Available,
    NotCollected,
    Busy,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCounterKind {
    Count,
    BillingBytes,
    BudgetBytes,
}

/// 计数与计费值不是堆内存实测，不参与 RSS 分项。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryCounter {
    pub key: String,
    pub value: DecimalU64,
    pub kind: MemoryCounterKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryRuleRow {
    pub resource: String,
    pub selector: Option<String>,
    pub rule_count: DecimalU64,
    pub estimated_bytes: DecimalU64,
    pub holders: Vec<String>,
    pub retained: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryModule {
    pub id: MemoryModuleId,
    pub state: MemoryProbeState,
    /// 只有有明确算法的载荷估算才有值；未采集不是零。
    pub estimated_bytes: Option<DecimalU64>,
    pub estimate_version: Option<String>,
    pub counters: Vec<MemoryCounter>,
    pub rules: Vec<MemoryRuleRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySnapshot {
    pub snapshot_id: DecimalU64,
    pub sampled_at_ms: u64,
    pub runtime_revision: DecimalU64,
    pub modules: Vec<MemoryModule>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCollectionState {
    Idle,
    Collecting,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryCollectionFailure {
    Timeout,
    SamplingFailed,
    RuntimeChanged,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryAnalysis {
    pub process_id: u32,
    pub started_at_ms: u64,
    pub sample_interval_seconds: u32,
    pub gap_after_seconds: u32,
    pub os: MemoryOsSample,
    pub trend: Vec<MemoryOsSample>,
    pub snapshot: Option<MemorySnapshot>,
    pub previous_snapshot: Option<MemorySnapshot>,
    pub collection_state: MemoryCollectionState,
    pub last_failure: Option<MemoryCollectionFailure>,
}
