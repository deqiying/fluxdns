//! 不可变资源快照和按资源 CAS 发布。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use crate::config::resolve::ConfigId;

/// 资源内容的来源类型。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceSourceKind {
    Const,
    File,
    Remote,
}

/// 资源快照的过期状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceStaleStatus {
    Fresh,
    Stale,
}

/// 资源版本，用于资源级 epoch/revision 比较和 CAS。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ResourceVersion {
    epoch: u64,
    revision: u64,
}

impl ResourceVersion {
    pub const fn new(epoch: u64, revision: u64) -> Self {
        Self { epoch, revision }
    }

    pub const fn epoch(self) -> u64 {
        self.epoch
    }

    pub const fn revision(self) -> u64 {
        self.revision
    }
}

/// 单个资源的不可变元数据和已编译结果。
#[derive(Clone)]
pub struct ResourceSnapshot<T = ()> {
    resource_id: ConfigId,
    epoch: u64,
    revision: u64,
    content_hash: Arc<str>,
    source_fingerprint: Arc<str>,
    parser_version: Arc<str>,
    fetched_at: SystemTime,
    source_kind: ResourceSourceKind,
    used_fallback: bool,
    stale_status: ResourceStaleStatus,
    compiled: Arc<T>,
}

impl<T> ResourceSnapshot<T> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        resource_id: ConfigId,
        epoch: u64,
        revision: u64,
        content_hash: impl Into<Arc<str>>,
        source_fingerprint: impl Into<Arc<str>>,
        parser_version: impl Into<Arc<str>>,
        fetched_at: SystemTime,
        source_kind: ResourceSourceKind,
        used_fallback: bool,
        stale_status: ResourceStaleStatus,
        compiled: T,
    ) -> Self {
        Self {
            resource_id,
            epoch,
            revision,
            content_hash: content_hash.into(),
            source_fingerprint: source_fingerprint.into(),
            parser_version: parser_version.into(),
            fetched_at,
            source_kind,
            used_fallback,
            stale_status,
            compiled: Arc::new(compiled),
        }
    }

    pub fn resource_id(&self) -> &ConfigId {
        &self.resource_id
    }

    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn version(&self) -> ResourceVersion {
        ResourceVersion::new(self.epoch, self.revision)
    }

    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    pub fn source_fingerprint(&self) -> &str {
        &self.source_fingerprint
    }

    pub fn parser_version(&self) -> &str {
        &self.parser_version
    }

    pub const fn fetched_at(&self) -> SystemTime {
        self.fetched_at
    }

    pub const fn source_kind(&self) -> ResourceSourceKind {
        self.source_kind
    }

    pub const fn used_fallback(&self) -> bool {
        self.used_fallback
    }

    pub const fn stale_status(&self) -> ResourceStaleStatus {
        self.stale_status
    }

    pub fn compiled(&self) -> &T {
        &self.compiled
    }

    pub fn compiled_arc(&self) -> Arc<T> {
        Arc::clone(&self.compiled)
    }
}

impl<T> fmt::Debug for ResourceSnapshot<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceSnapshot")
            .field("resource_id", &"[REDACTED]")
            .field("epoch", &self.epoch)
            .field("revision", &self.revision)
            .field("content_hash", &"[REDACTED]")
            .field("source_fingerprint", &"[REDACTED]")
            .field("parser_version", &self.parser_version)
            .field("fetched_at", &self.fetched_at)
            .field("source_kind", &self.source_kind)
            .field("used_fallback", &self.used_fallback)
            .field("stale_status", &self.stale_status)
            .field("compiled", &"[REDACTED]")
            .finish()
    }
}

/// 发布候选被当前资源版本拒绝的原因。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourcePublishError {
    StaleEpoch {
        resource_id: ResourceVersion,
        candidate: ResourceVersion,
    },
    CompareAndSwapFailed {
        expected: Option<ResourceVersion>,
        actual: Option<ResourceVersion>,
    },
}

/// 供管理面和观测面使用的安全资源摘要。
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ResourceSummary {
    version: ResourceVersion,
    source_kind: ResourceSourceKind,
    used_fallback: bool,
    stale_status: ResourceStaleStatus,
}

impl ResourceSummary {
    pub const fn version(self) -> ResourceVersion {
        self.version
    }

    pub const fn source_kind(self) -> ResourceSourceKind {
        self.source_kind
    }

    pub const fn used_fallback(self) -> bool {
        self.used_fallback
    }

    pub const fn stale_status(self) -> ResourceStaleStatus {
        self.stale_status
    }
}

impl fmt::Debug for ResourceSummary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceSummary")
            .field("version", &self.version)
            .field("source_kind", &self.source_kind)
            .field("used_fallback", &self.used_fallback)
            .field("stale_status", &self.stale_status)
            .finish()
    }
}

/// 当前资源集合的不可变视图。
#[derive(Clone)]
pub struct ResourceRegistrySnapshot<T = ()> {
    resources: BTreeMap<ConfigId, Arc<ResourceSnapshot<T>>>,
}

impl<T> Default for ResourceRegistrySnapshot<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ResourceRegistrySnapshot<T> {
    pub const fn new() -> Self {
        Self {
            resources: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.resources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    pub fn lookup(&self, resource_id: &ConfigId) -> Option<Arc<ResourceSnapshot<T>>> {
        self.resources.get(resource_id).cloned()
    }

    pub fn summary(&self) -> Vec<(ConfigId, ResourceSummary)> {
        self.resources
            .iter()
            .map(|(resource_id, snapshot)| {
                (
                    resource_id.clone(),
                    ResourceSummary {
                        version: snapshot.version(),
                        source_kind: snapshot.source_kind,
                        used_fallback: snapshot.used_fallback,
                        stale_status: snapshot.stale_status,
                    },
                )
            })
            .collect()
    }

    /// 将另一个 registry 中允许的更高版本合并进当前候选。
    ///
    /// 当前 registry 代表待发布候选，因此同版本或更高版本始终保留候选内容；
    /// 只有 incoming 资源版本严格更高时才替换。调用方通过 `allowed` 过滤已经
    /// 从新配置删除或定义不兼容的资源。
    pub fn merge_newer_from<F>(&self, incoming: &Self, mut allowed: F) -> Self
    where
        F: FnMut(&ConfigId) -> bool,
    {
        let mut resources = self.resources.clone();
        for (resource_id, incoming_snapshot) in &incoming.resources {
            if !allowed(resource_id) {
                continue;
            }
            let should_replace = resources
                .get(resource_id)
                .is_none_or(|current| incoming_snapshot.version() > current.version());
            if should_replace {
                resources.insert(resource_id.clone(), Arc::clone(incoming_snapshot));
            }
        }
        Self { resources }
    }

    /// 发布一个资源，并返回保留其他资源的新 registry。
    pub fn publish(&self, candidate: ResourceSnapshot<T>) -> Result<Self, ResourcePublishError> {
        let resource_id = candidate.resource_id().clone();
        let candidate = Arc::new(candidate);
        self.publish_arc(candidate, resource_id, None)
    }

    /// 对指定资源执行基于当前 immutable registry 的 CAS 发布。
    pub fn compare_and_publish(
        &self,
        expected: Option<ResourceVersion>,
        candidate: ResourceSnapshot<T>,
    ) -> Result<Self, ResourcePublishError> {
        let resource_id = candidate.resource_id().clone();
        let candidate = Arc::new(candidate);
        self.publish_arc(candidate, resource_id, Some(expected))
    }

    fn publish_arc(
        &self,
        candidate: Arc<ResourceSnapshot<T>>,
        resource_id: ConfigId,
        expected: Option<Option<ResourceVersion>>,
    ) -> Result<Self, ResourcePublishError> {
        let current = self.resources.get(&resource_id);
        let actual = current.map(|snapshot| snapshot.version());
        if let Some(expected) = expected
            && expected != actual
        {
            return Err(ResourcePublishError::CompareAndSwapFailed { expected, actual });
        }

        if let Some(current) = current {
            let candidate_version = candidate.version();
            if candidate.epoch() <= current.epoch() {
                return Err(ResourcePublishError::StaleEpoch {
                    resource_id: current.version(),
                    candidate: candidate_version,
                });
            }
        }

        let mut resources = self.resources.clone();
        resources.insert(resource_id, candidate);
        Ok(Self { resources })
    }
}

impl<T> fmt::Debug for ResourceRegistrySnapshot<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceRegistrySnapshot")
            .field("resource_count", &self.resources.len())
            .field(
                "versions",
                &self
                    .resources
                    .values()
                    .map(|snapshot| snapshot.version())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;

    fn id(value: &str) -> ConfigId {
        ConfigId::new(value).expect("valid resource id")
    }

    fn snapshot(name: &str, epoch: u64, revision: u64, value: &str) -> ResourceSnapshot<String> {
        ResourceSnapshot::new(
            id(name),
            epoch,
            revision,
            format!("hash-{value}"),
            format!("fingerprint-{value}"),
            "parser-v1",
            UNIX_EPOCH,
            ResourceSourceKind::File,
            false,
            ResourceStaleStatus::Fresh,
            value.to_owned(),
        )
    }

    #[test]
    fn publish_replaces_same_resource_only_with_newer_epoch() {
        let registry = ResourceRegistrySnapshot::new()
            .publish(snapshot("hosts", 1, 1, "old"))
            .unwrap();
        let rejected = registry.publish(snapshot("hosts", 1, 2, "same-epoch"));
        assert!(matches!(
            rejected,
            Err(ResourcePublishError::StaleEpoch { .. })
        ));

        let next = registry.publish(snapshot("hosts", 2, 3, "new")).unwrap();
        assert_eq!(next.lookup(&id("hosts")).unwrap().compiled(), "new");
        assert_eq!(next.len(), 1);
    }

    #[test]
    fn publishing_different_resources_preserves_existing_entries() {
        let first = ResourceRegistrySnapshot::new()
            .publish(snapshot("hosts", 1, 1, "hosts"))
            .unwrap();
        let second = first.publish(snapshot("rules", 4, 1, "rules")).unwrap();

        assert_eq!(second.len(), 2);
        assert_eq!(second.lookup(&id("hosts")).unwrap().compiled(), "hosts");
        assert_eq!(second.lookup(&id("rules")).unwrap().compiled(), "rules");
        assert_eq!(
            second
                .summary()
                .into_iter()
                .map(|(id, _)| id.as_str().to_owned())
                .collect::<Vec<_>>(),
            vec!["hosts", "rules"]
        );
    }

    #[test]
    fn merge_newer_from_preserves_candidate_on_equal_or_newer_versions() {
        let candidate = ResourceRegistrySnapshot::new()
            .publish(snapshot("same", 2, 9, "candidate-same"))
            .unwrap()
            .publish(snapshot("newer", 4, 1, "candidate-newer"))
            .unwrap();
        let incoming = ResourceRegistrySnapshot::new()
            .publish(snapshot("same", 2, 9, "incoming-same"))
            .unwrap()
            .publish(snapshot("newer", 3, 9, "incoming-older"))
            .unwrap()
            .publish(snapshot("older", 5, 1, "incoming-older-id"))
            .unwrap();

        let merged = candidate.merge_newer_from(&incoming, |_| true);

        assert_eq!(
            merged.lookup(&id("same")).unwrap().compiled(),
            "candidate-same"
        );
        assert_eq!(
            merged.lookup(&id("newer")).unwrap().compiled(),
            "candidate-newer"
        );
        assert_eq!(
            merged.lookup(&id("older")).unwrap().compiled(),
            "incoming-older-id"
        );
    }

    #[test]
    fn merge_newer_from_filters_incoming_resources() {
        let candidate = ResourceRegistrySnapshot::new()
            .publish(snapshot("kept", 1, 1, "candidate"))
            .unwrap();
        let incoming = ResourceRegistrySnapshot::new()
            .publish(snapshot("kept", 2, 1, "incoming-kept"))
            .unwrap()
            .publish(snapshot("filtered", 9, 1, "incoming-filtered"))
            .unwrap();

        let merged =
            candidate.merge_newer_from(&incoming, |resource_id| resource_id == &id("kept"));

        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged.lookup(&id("kept")).unwrap().compiled(),
            "incoming-kept"
        );
        assert!(merged.lookup(&id("filtered")).is_none());
    }

    #[test]
    fn compare_and_publish_requires_expected_version() {
        let registry = ResourceRegistrySnapshot::new()
            .publish(snapshot("hosts", 1, 1, "old"))
            .unwrap();
        let failed = registry.compare_and_publish(
            Some(ResourceVersion::new(9, 9)),
            snapshot("hosts", 2, 2, "new"),
        );
        assert!(matches!(
            failed,
            Err(ResourcePublishError::CompareAndSwapFailed {
                expected: Some(ResourceVersion {
                    epoch: 9,
                    revision: 9
                }),
                actual: Some(ResourceVersion {
                    epoch: 1,
                    revision: 1
                }),
            })
        ));

        let next = registry
            .compare_and_publish(
                Some(ResourceVersion::new(1, 1)),
                snapshot("hosts", 2, 2, "new"),
            )
            .unwrap();
        assert_eq!(next.lookup(&id("hosts")).unwrap().compiled(), "new");
    }

    #[test]
    fn debug_redacts_identifying_content() {
        let snapshot = snapshot("private-resource", 1, 1, "secret-domain-content");
        let rendered = format!("{snapshot:?}");
        assert!(!rendered.contains("private-resource"));
        assert!(!rendered.contains("secret-domain-content"));
        assert!(!rendered.contains("hash-secret-domain-content"));
        assert!(rendered.contains("REDACTED"));
    }
}
