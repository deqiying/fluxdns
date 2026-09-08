//! 统计与详情共用的单调保留水位计算、发布和恢复边界。

use std::sync::Arc;
use std::time::{Instant, SystemTime};

use thiserror::Error;

use crate::dns::Deadline;
use crate::ports::{PortError, PortErrorClass};

use super::{
    DetailShardStore, SqliteStorageBackend, StatsPersistenceError, StatsPersistenceWorker,
};

pub const DEFAULT_RETENTION_DAYS: u32 = 7;
pub const DEFAULT_RETENTION_GRACE_DAYS: u32 = 3;
pub const DEFAULT_RETENTION_REFERENCE_SIZE_BYTES: u64 = 1 << 30;
pub const MAX_RETENTION_DAYS: u32 = 3_650;
pub const MAX_RETENTION_REFERENCE_SIZE_BYTES: u64 = 1 << 40;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPolicy {
    pub days: u32,
    pub grace_days: u32,
    pub reference_size_bytes: u64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            days: DEFAULT_RETENTION_DAYS,
            grace_days: DEFAULT_RETENTION_GRACE_DAYS,
            reference_size_bytes: DEFAULT_RETENTION_REFERENCE_SIZE_BYTES,
        }
    }
}

impl RetentionPolicy {
    pub fn new(
        days: u32,
        grace_days: u32,
        reference_size_bytes: u64,
    ) -> Result<Self, RetentionPolicyError> {
        if days == 0
            || days > MAX_RETENTION_DAYS
            || days
                .checked_add(grace_days)
                .is_none_or(|total| total > MAX_RETENTION_DAYS)
            || !(1..=MAX_RETENTION_REFERENCE_SIZE_BYTES).contains(&reference_size_bytes)
        {
            return Err(RetentionPolicyError::Invalid);
        }
        Ok(Self {
            days,
            grace_days,
            reference_size_bytes,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum RetentionPolicyError {
    #[error("invalid retention policy")]
    Invalid,
    #[error("retention cutoff is outside the supported UTC day range")]
    DayOutOfRange,
    #[error("sampled detail size exceeds the persisted integer range")]
    SampleTooLarge,
}

/// 一轮任务冻结的输入和计算结果；`S == T` 仍使用宽限期。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionPlan {
    pub policy: RetentionPolicy,
    pub reference_day_utc: i32,
    pub sampled_detail_bytes: u64,
    pub target_days: u32,
    pub keep_from_day_utc: i32,
}

impl RetentionPlan {
    pub fn calculate(
        policy: RetentionPolicy,
        reference_day_utc: i32,
        sampled_detail_bytes: u64,
    ) -> Result<Self, RetentionPolicyError> {
        let policy =
            RetentionPolicy::new(policy.days, policy.grace_days, policy.reference_size_bytes)?;
        if i64::try_from(sampled_detail_bytes).is_err() {
            return Err(RetentionPolicyError::SampleTooLarge);
        }
        let target_days = if sampled_detail_bytes > policy.reference_size_bytes {
            policy.days
        } else {
            policy
                .days
                .checked_add(policy.grace_days)
                .ok_or(RetentionPolicyError::Invalid)?
        };
        let days_before = i32::try_from(target_days.saturating_sub(1))
            .map_err(|_| RetentionPolicyError::DayOutOfRange)?;
        let keep_from_day_utc = reference_day_utc
            .checked_sub(days_before)
            .ok_or(RetentionPolicyError::DayOutOfRange)?;
        // 复用分片日期编码范围，避免生成永远无法定位的水位。
        if super::detail_shards::format_shard_file_name(reference_day_utc).is_none()
            || super::detail_shards::format_shard_file_name(keep_from_day_utc).is_none()
        {
            return Err(RetentionPolicyError::DayOutOfRange);
        }
        Ok(Self {
            policy,
            reference_day_utc,
            sampled_detail_bytes,
            target_days,
            keep_from_day_utc,
        })
    }

    pub(crate) fn is_valid(&self) -> bool {
        Self::calculate(
            self.policy,
            self.reference_day_utc,
            self.sampled_detail_bytes,
        )
        .is_ok_and(|expected| expected == *self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionState {
    pub revision: u64,
    pub watermark_revision: u64,
    pub retired_before_day_utc: i32,
    pub reference_day_utc: i32,
    pub target_days: u32,
    pub sampled_detail_bytes: u64,
    pub reference_size_bytes: u64,
    pub replay_floor_batch_id: u64,
    pub published_at: SystemTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RetentionBootstrap {
    pub state: Option<RetentionState>,
    pub next_stats_batch_id: u64,
}

#[derive(Debug, Error)]
pub enum RetentionError {
    #[error("retention policy calculation failed: {0}")]
    Policy(#[source] RetentionPolicyError),
    #[error("retention detail sampling failed: {0}")]
    Sample(#[source] PortError),
    #[error("retention stats boundary failed: {0}")]
    Stats(#[source] StatsPersistenceError),
    #[error("retention detail boundary failed: {0}")]
    Detail(#[source] PortError),
    #[error("retention state transaction failed: {0}")]
    Backend(#[source] PortError),
}

/// 单次保留任务 owner；调度、预览确认和物理回收状态由 BC-11 在此边界外接入。
pub struct RetentionCoordinator {
    backend: Arc<SqliteStorageBackend>,
    stats: Arc<StatsPersistenceWorker>,
    detail: Arc<DetailShardStore>,
    run_lock: tokio::sync::Mutex<()>,
}

impl RetentionCoordinator {
    pub(crate) fn new(
        backend: Arc<SqliteStorageBackend>,
        stats: Arc<StatsPersistenceWorker>,
        detail: Arc<DetailShardStore>,
    ) -> Self {
        Self {
            backend,
            stats,
            detail,
            run_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 仅采样并计算，不发布水位或创建任务记录。
    pub async fn preview(
        &self,
        policy: RetentionPolicy,
        reference_day_utc: i32,
        deadline: Deadline,
    ) -> Result<RetentionPlan, RetentionError> {
        let sample = self
            .detail
            .sample_managed_storage(deadline)
            .map_err(RetentionError::Sample)?;
        RetentionPlan::calculate(policy, reference_day_utc, sample.bytes)
            .map_err(RetentionError::Policy)
    }

    /// 发布 stats 事务水位；成功后在同一详情 write guard 下更新进程可见边界。
    pub async fn publish(
        &self,
        plan: RetentionPlan,
        deadline: Deadline,
    ) -> Result<RetentionState, RetentionError> {
        let _run = tokio::time::timeout(deadline.remaining(Instant::now()), self.run_lock.lock())
            .await
            .map_err(|_| {
                RetentionError::Backend(PortError::new(
                    PortErrorClass::Timeout,
                    "retention.run_lock",
                ))
            })?;
        let detail = self
            .detail
            .begin_retention_publication(deadline)
            .await
            .map_err(RetentionError::Detail)?;
        // 在 write guard 内重新枚举文件，避免预览后完成的迟到写入漏入 manifest；S 和截止线仍使用冻结 plan。
        let manifest_days = self
            .detail
            .sample_managed_storage(deadline)
            .map_err(RetentionError::Sample)?
            .shard_days;
        let stats = self
            .stats
            .begin_retention(deadline)
            .await
            .map_err(RetentionError::Stats)?;
        let state = self
            .backend
            .publish_retention(
                plan,
                &manifest_days,
                stats.replay_floor_batch_id(),
                deadline,
            )
            .await
            .map_err(RetentionError::Backend)?;
        detail.publish(state.retired_before_day_utc);
        stats.commit();
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, UNIX_EPOCH};

    use crate::dns::{Deadline, RuntimeRevision, TransportClass};
    use crate::ports::observation::ClientMatchSource;
    use crate::ports::storage::{ResolveEvent, StatsSource, StorageBackend};
    use crate::ports::telemetry::{CacheStatus, OutcomeClass};
    use crate::storage::sqlite::InjectedSqliteFault;
    use crate::storage::{
        DetailPageDirection, DetailQuery, DetailQueryFilter, DetailQuerySort, DetailShardStore,
        DetailSortOrder, ResolveDetailRecord, SqliteStorageBackend, StatsPersistenceWorker,
    };

    use super::{RetentionCoordinator, RetentionPlan, RetentionPolicy, RetentionPolicyError};

    const DAY_MILLIS: u64 = 86_400_000;
    const REFERENCE_DAY: i32 = 20_710;
    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    fn test_root(name: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("_fluxdns")
            .join("p2-retention-tests")
            .join(format!(
                "{name}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn detail_record(day_utc: i32, qname: &str) -> ResolveDetailRecord {
        ResolveDetailRecord::from_event(ResolveEvent {
            occurred_at: UNIX_EPOCH
                + Duration::from_millis(u64::try_from(day_utc).unwrap() * DAY_MILLIS + 1),
            duration_millis: 2,
            dns_core_duration_micros: 800,
            request_digest: Arc::from("retention-test-digest"),
            listener_id: Arc::from("udp-main"),
            route_id: None,
            client_id: Some(Arc::from("raw-client")),
            client_ip: Some("192.0.2.10".parse().unwrap()),
            client_match_source: Some(ClientMatchSource::Id),
            matched_client_id: Some(Arc::from("client-a")),
            client_bucket: Some(Arc::from("client-a")),
            strategy_id: Some(Arc::from("default")),
            upstream_id: Some(Arc::from("public")),
            upstream_member_id: None,
            upstream_used_id: Some(Arc::from("alidns")),
            matched_rule_source: None,
            matched_resource_id: None,
            matched_rule_ordinal: None,
            resource_version: None,
            transport: TransportClass::Datagram,
            qname: Arc::from(qname),
            qtype: 1,
            qclass: 1,
            answers: Vec::new(),
            rcode: 0,
            cancellation_reason: None,
            outcome: OutcomeClass::Success,
            source: StatsSource::Upstream,
            cache_status: CacheStatus::Miss,
            runtime_revision: RuntimeRevision(1),
        })
        .unwrap()
    }

    fn query(day_utc: i32) -> DetailQuery {
        DetailQuery {
            filter: DetailQueryFilter {
                from_utc_millis: u64::try_from(day_utc).unwrap() * DAY_MILLIS,
                to_utc_millis: u64::try_from(day_utc + 1).unwrap() * DAY_MILLIS,
                ..DetailQueryFilter::default()
            },
            cursor: None,
            direction: DetailPageDirection::Older,
            page_size: 20,
            sort: DetailQuerySort::OccurredAt,
            order: DetailSortOrder::Asc,
        }
    }

    #[test]
    fn calculation_uses_grace_at_threshold_and_includes_reference_day() {
        for (days, grace, size, threshold, expected_target, expected_keep) in [
            (1, 0, 0, 1, 1, REFERENCE_DAY),
            (3, 2, 9, 10, 5, REFERENCE_DAY - 4),
            (7, 3, 10, 10, 10, REFERENCE_DAY - 9),
            (7, 3, 11, 10, 7, REFERENCE_DAY - 6),
            (30, 0, 1 << 40, 1, 30, REFERENCE_DAY - 29),
        ] {
            let policy = RetentionPolicy::new(days, grace, threshold).unwrap();
            let plan = RetentionPlan::calculate(policy, REFERENCE_DAY, size).unwrap();
            assert_eq!(plan.target_days, expected_target);
            assert_eq!(plan.keep_from_day_utc, expected_keep);
        }
        for invalid in [
            RetentionPolicy::new(0, 0, 1),
            RetentionPolicy::new(3_650, 1, 1),
            RetentionPolicy::new(1, 0, 0),
            RetentionPolicy::new(1, 0, (1_u64 << 40) + 1),
        ] {
            assert_eq!(invalid.unwrap_err(), RetentionPolicyError::Invalid);
        }
        assert_eq!(
            RetentionPlan::calculate(RetentionPolicy::default(), i32::MIN, 0).unwrap_err(),
            RetentionPolicyError::DayOutOfRange
        );
        assert_eq!(
            RetentionPlan::calculate(RetentionPolicy::default(), REFERENCE_DAY, u64::MAX)
                .unwrap_err(),
            RetentionPolicyError::SampleTooLarge
        );
    }

    #[tokio::test]
    async fn sample_counts_only_managed_main_and_wal_files() {
        let root = test_root("sample");
        let store = DetailShardStore::new(root.clone(), Vec::new(), 1).unwrap();
        store
            .write_records(
                REFERENCE_DAY,
                &[detail_record(REFERENCE_DAY, "sample.example.")],
                deadline(),
            )
            .await
            .unwrap();
        let main = store.shard_path(REFERENCE_DAY).unwrap();
        let main_bytes = std::fs::metadata(&main).unwrap().len();
        std::fs::write(format!("{}-wal", main.display()), b"wal").unwrap();
        std::fs::write(format!("{}-shm", main.display()), b"ignored-shm").unwrap();
        std::fs::write(root.join("backup.sqlite3.bak"), b"ignored-backup").unwrap();
        let sample = store.sample_managed_storage(deadline()).unwrap();
        assert_eq!(sample.bytes, main_bytes + 3);
        assert_eq!(sample.shard_days, [REFERENCE_DAY]);
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn real_sqlite_publication_is_monotonic_and_guards_replay_and_late_writes() {
        let root = test_root("publish");
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("stats.sqlite3");
        let details = root.join("queries");
        let backend = Arc::new(
            SqliteStorageBackend::connect_with_deadline(database.clone(), deadline())
                .await
                .unwrap(),
        );
        backend
            .migrate(crate::storage::STORAGE_SCHEMA_VERSION, deadline())
            .await
            .unwrap();
        let bootstrap = backend.retention_bootstrap(deadline()).await.unwrap();
        assert!(bootstrap.state.is_none());
        assert_eq!(bootstrap.next_stats_batch_id, 1);
        let stats = Arc::new(StatsPersistenceWorker::with_next_batch_id(
            backend.clone(),
            bootstrap.next_stats_batch_id,
        ));
        let detail =
            Arc::new(DetailShardStore::new(details.clone(), vec![database.clone()], 2).unwrap());
        for day in [REFERENCE_DAY - 3, REFERENCE_DAY - 2, REFERENCE_DAY] {
            detail
                .write_records(day, &[detail_record(day, "kept.example.")], deadline())
                .await
                .unwrap();
            stats.record_request(day, Vec::new()).unwrap();
        }
        assert_eq!(stats.flush(deadline()).await.unwrap().events_committed, 3);
        // 先制造真实 pending batch；水位发布后重放只能确认 ledger，不能恢复业务统计。
        stats.record_request(REFERENCE_DAY - 4, Vec::new()).unwrap();
        backend.inject_fault(InjectedSqliteFault::Busy);
        assert!(stats.flush(deadline()).await.is_err());
        assert_eq!(stats.pending_batch_count(), 1);

        let coordinator = Arc::new(RetentionCoordinator::new(
            backend.clone(),
            stats.clone(),
            detail.clone(),
        ));
        let policy = RetentionPolicy::new(3, 0, 1 << 40).unwrap();
        let plan = coordinator
            .preview(policy, REFERENCE_DAY, deadline())
            .await
            .unwrap();
        assert_eq!(plan.keep_from_day_utc, REFERENCE_DAY - 2);
        let held_lease = detail
            .acquire_read(REFERENCE_DAY - 3, deadline())
            .await
            .unwrap()
            .unwrap();
        let publish_coordinator = Arc::clone(&coordinator);
        let publication =
            tokio::spawn(async move { publish_coordinator.publish(plan, deadline()).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!publication.is_finished());
        held_lease.close(deadline()).await.unwrap();
        let state = publication.await.unwrap().unwrap();
        assert_eq!(state.revision, 1);
        assert_eq!(state.watermark_revision, 1);
        assert_eq!(state.retired_before_day_utc, REFERENCE_DAY - 2);

        let verification = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&database)
                    .read_only(true),
            )
            .await
            .unwrap();
        let totals: Vec<i64> =
            sqlx::query_scalar("SELECT day_utc FROM stats_daily_total ORDER BY day_utc")
                .fetch_all(&verification)
                .await
                .unwrap();
        assert_eq!(
            totals,
            [i64::from(REFERENCE_DAY - 2), i64::from(REFERENCE_DAY)]
        );
        let manifest: Vec<(i64, String)> =
            sqlx::query_as("SELECT day_utc, state FROM retention_detail_manifest ORDER BY day_utc")
                .fetch_all(&verification)
                .await
                .unwrap();
        assert_eq!(manifest, [(i64::from(REFERENCE_DAY - 3), "pending".into())]);
        let ledger_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM stats_batch_ledger")
            .fetch_one(&verification)
            .await
            .unwrap();
        assert_eq!(ledger_count, 0, "confirmed replay prefix must be reclaimed");
        assert!(
            detail
                .query_details(query(REFERENCE_DAY - 3), deadline())
                .await
                .unwrap()
                .items
                .is_empty()
        );
        let dropped = detail
            .write_records(
                REFERENCE_DAY - 3,
                &[detail_record(REFERENCE_DAY - 3, "late.example.")],
                deadline(),
            )
            .await
            .unwrap();
        assert_eq!(dropped.dropped, 1);
        assert_eq!(stats.flush(deadline()).await.unwrap().events_committed, 1);
        assert_eq!(stats.pending_batch_count(), 0);
        let retired_total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM stats_daily_total WHERE day_utc < ?")
                .bind(i64::from(REFERENCE_DAY - 2))
                .fetch_one(&verification)
                .await
                .unwrap();
        assert_eq!(retired_total, 0);

        let expanded = RetentionPlan::calculate(
            RetentionPolicy::new(30, 0, 1 << 40).unwrap(),
            REFERENCE_DAY,
            state.sampled_detail_bytes,
        )
        .unwrap();
        let expanded_state = coordinator.publish(expanded, deadline()).await.unwrap();
        assert_eq!(expanded_state.revision, 2);
        assert_eq!(expanded_state.watermark_revision, 1);
        assert_eq!(
            expanded_state.retired_before_day_utc,
            state.retired_before_day_utc
        );
        verification.close().await;
        detail.shutdown(deadline()).await.unwrap();
        backend.shutdown(deadline()).await.unwrap();
        drop((coordinator, stats, detail, backend));

        let reopened = Arc::new(
            SqliteStorageBackend::connect_with_deadline(database.clone(), deadline())
                .await
                .unwrap(),
        );
        let bootstrap = reopened.retention_bootstrap(deadline()).await.unwrap();
        assert_eq!(
            bootstrap.state.unwrap().retired_before_day_utc,
            REFERENCE_DAY - 2
        );
        assert_eq!(bootstrap.next_stats_batch_id, 3);
        let reopened_stats = StatsPersistenceWorker::with_next_batch_id(
            reopened.clone(),
            bootstrap.next_stats_batch_id,
        );
        reopened_stats
            .record_request(REFERENCE_DAY - 5, Vec::new())
            .unwrap();
        reopened_stats.flush(deadline()).await.unwrap();
        let reopened_verification = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(&database)
                    .read_only(true),
            )
            .await
            .unwrap();
        let batch_id: i64 = sqlx::query_scalar("SELECT MAX(batch_id) FROM stats_batch_ledger")
            .fetch_one(&reopened_verification)
            .await
            .unwrap();
        assert_eq!(batch_id, 3);
        reopened_verification.close().await;
        reopened.shutdown(deadline()).await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_stats_transaction_does_not_publish_detail_watermark() {
        let root = test_root("rollback");
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("stats.sqlite3");
        let backend = Arc::new(
            SqliteStorageBackend::connect_with_deadline(database.clone(), deadline())
                .await
                .unwrap(),
        );
        let bootstrap = backend.retention_bootstrap(deadline()).await.unwrap();
        let stats = Arc::new(StatsPersistenceWorker::with_next_batch_id(
            backend.clone(),
            bootstrap.next_stats_batch_id,
        ));
        let detail = Arc::new(
            DetailShardStore::new(root.join("queries"), vec![database.clone()], 1).unwrap(),
        );
        let old_day = REFERENCE_DAY - 5;
        detail
            .write_records(
                old_day,
                &[detail_record(old_day, "old.example.")],
                deadline(),
            )
            .await
            .unwrap();
        stats.record_request(old_day, Vec::new()).unwrap();
        stats.flush(deadline()).await.unwrap();
        let sabotage = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&database))
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER reject_stats_retention BEFORE DELETE ON stats_daily_total \
             BEGIN SELECT RAISE(ABORT, 'reject retention'); END",
        )
        .execute(&sabotage)
        .await
        .unwrap();
        let coordinator = RetentionCoordinator::new(backend.clone(), stats.clone(), detail.clone());
        let plan = RetentionPlan::calculate(
            RetentionPolicy::new(1, 0, 1 << 40).unwrap(),
            REFERENCE_DAY,
            0,
        )
        .unwrap();
        assert!(coordinator.publish(plan, deadline()).await.is_err());
        assert_eq!(detail.retired_before(), None);
        assert_eq!(
            detail
                .query_details(query(old_day), deadline())
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        let state_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM retention_state")
            .fetch_one(&sabotage)
            .await
            .unwrap();
        assert_eq!(state_count, 0);
        sqlx::query("DROP TRIGGER reject_stats_retention")
            .execute(&sabotage)
            .await
            .unwrap();
        sabotage.close().await;
        detail.shutdown(deadline()).await.unwrap();
        backend.shutdown(deadline()).await.unwrap();
        drop((coordinator, stats, detail, backend));
        std::fs::remove_dir_all(root).unwrap();
    }
}
