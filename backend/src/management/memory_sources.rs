//! 从进程级真实 owner 读取计数，不维护影子统计或触发存储维护。

use std::sync::Arc;

use crate::resolution::ResolutionPipelineMetrics;
use crate::storage::{DetailShardStore, StatsPersistenceWorker};

use super::contract::{
    MemoryCounter, MemoryCounterKind, MemoryModule, MemoryModuleId, MemoryProbeState,
};

pub(crate) struct MemoryRuntimeSources {
    stats: Arc<StatsPersistenceWorker>,
    resolution: Arc<ResolutionPipelineMetrics>,
    detail: Arc<DetailShardStore>,
}

impl MemoryRuntimeSources {
    /// 保留生产 owner，队列自身仅由 metrics 内的弱引用探测。
    pub(crate) fn new(
        stats: Arc<StatsPersistenceWorker>,
        resolution: Arc<ResolutionPipelineMetrics>,
        detail: Arc<DetailShardStore>,
    ) -> Self {
        Self {
            stats,
            resolution,
            detail,
        }
    }

    /// 锁竞争时整组标忙且省略计数；计数不换算为堆内存字节。
    pub(crate) fn modules(&self) -> Vec<MemoryModule> {
        let mut statistics = module(MemoryModuleId::StatisticsQueues);
        match (
            self.stats.try_memory_counts(),
            self.resolution.try_memory_counts(),
        ) {
            (Some((active, batches, events)), Some(queues)) => {
                statistics.counters = vec![
                    counter("active_events", active),
                    counter("pending_batches", batches),
                    counter("pending_events", events),
                ];
                for (key, value) in [
                    ("ingress_queue", queues.ingress),
                    ("cache_commit_queue", queues.cache),
                    ("detail_projection_queue", queues.detail),
                ] {
                    if let Some(value) = value {
                        statistics.counters.push(counter(key, value));
                    }
                }
                statistics.state = MemoryProbeState::Available;
            }
            _ => statistics.state = MemoryProbeState::Busy,
        }
        let mut sqlite = module(MemoryModuleId::Sqlite);
        let (active, budget) = self.detail.memory_lease_counts();
        sqlite.counters = vec![
            counter("active_leases", active),
            counter("lease_budget", budget),
        ];
        sqlite.state = MemoryProbeState::Available;
        vec![statistics, sqlite]
    }
}

fn module(id: MemoryModuleId) -> MemoryModule {
    MemoryModule {
        id,
        state: MemoryProbeState::NotCollected,
        estimated_bytes: None,
        estimate_version: None,
        counters: Vec::new(),
        rules: Vec::new(),
    }
}

fn counter(key: &str, value: u64) -> MemoryCounter {
    MemoryCounter {
        key: key.to_owned(),
        value: value.into(),
        kind: MemoryCounterKind::Count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::InMemoryStorageBackend;

    #[test]
    fn owner_counts_do_not_fabricate_bytes_or_disabled_queues() {
        let root =
            std::env::temp_dir().join(format!("fluxdns-memory-probe-{}", std::process::id()));
        let sources = MemoryRuntimeSources::new(
            Arc::new(StatsPersistenceWorker::new(Arc::new(
                InMemoryStorageBackend::new(),
            ))),
            Arc::new(ResolutionPipelineMetrics::default()),
            Arc::new(DetailShardStore::new(root.clone(), Vec::new(), 3).unwrap()),
        );
        let modules = sources.modules();
        assert_eq!(modules.len(), 2);
        for module in &modules {
            assert!(module.estimated_bytes.is_none());
            assert!(module.estimate_version.is_none());
            assert!(module.rules.is_empty());
            assert!(matches!(module.state, MemoryProbeState::Available));
            assert!(
                module
                    .counters
                    .iter()
                    .all(|c| matches!(c.kind, MemoryCounterKind::Count))
            );
        }
        assert_eq!(modules[0].counters.len(), 3);
        assert_eq!(modules[1].counters[1].value.as_u64(), 3);
        assert!(!root.exists(), "采集不得创建详情目录");
    }
}
