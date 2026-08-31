use std::fmt;
use std::net::SocketAddr;

use thiserror::Error;

use crate::config::{BindEntry, BindProtocol};
use crate::dns::{CancelReason, Cancellation, Deadline, RuntimeRevision};
use crate::ports::effects::{ActivatedSocket, SocketFactory, SocketKind, SocketSpec};
use crate::ports::{PortError, PortErrorClass};

use super::prepared::PreparedRuntime;
use super::snapshot::RuntimeSnapshot;

/// 所有 endpoint 都已准备并激活的监听集合。
pub struct BoundListenerSet {
    endpoints: Vec<BoundEndpoint>,
}

impl BoundListenerSet {
    fn new(endpoints: Vec<BoundEndpoint>) -> Self {
        Self { endpoints }
    }

    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = &BindEntry> {
        self.endpoints.iter().map(|endpoint| &endpoint.entry)
    }

    pub fn local_addrs(&self) -> Result<Vec<SocketAddr>, PortError> {
        self.endpoints
            .iter()
            .map(|endpoint| endpoint.socket.local_addr())
            .collect()
    }
}

impl fmt::Debug for BoundListenerSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundListenerSet")
            .field("endpoint_count", &self.endpoints.len())
            .field(
                "owners",
                &self
                    .endpoints
                    .iter()
                    .map(|item| &item.entry.owner)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

struct BoundEndpoint {
    entry: BindEntry,
    socket: Box<dyn ActivatedSocket>,
}

/// 已绑定但尚未交给 coordinator 发布的候选运行时。
pub struct BoundCandidate {
    prepared: PreparedRuntime,
    listeners: BoundListenerSet,
}

impl BoundCandidate {
    pub fn snapshot(&self) -> &RuntimeSnapshot {
        self.prepared.snapshot()
    }

    pub fn revision(&self) -> RuntimeRevision {
        self.snapshot().revision()
    }

    pub fn listeners(&self) -> &BoundListenerSet {
        &self.listeners
    }

    pub fn into_parts(self) -> (PreparedRuntime, BoundListenerSet) {
        (self.prepared, self.listeners)
    }
}

impl fmt::Debug for BoundCandidate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundCandidate")
            .field("revision", &self.revision())
            .field("listeners", &self.listeners)
            .finish()
    }
}

/// 绑定错误只携带安全的 endpoint 标识和 port 错误分类。
#[derive(Debug, Error)]
pub enum BindError {
    #[error("runtime bind was cancelled: {0:?}")]
    Cancelled(CancelReason),
    #[error("runtime bind deadline expired")]
    DeadlineExceeded,
    #[error("preparing bind entry {index} ({owner}) failed: {source}")]
    Prepare {
        index: usize,
        owner: String,
        #[source]
        source: PortError,
    },
    #[error("activating bind entry {index} ({owner}) failed: {source}")]
    Activate {
        index: usize,
        owner: String,
        #[source]
        source: PortError,
    },
}

impl BindError {
    pub fn class(&self) -> Option<&PortErrorClass> {
        match self {
            Self::Prepare { source, .. } | Self::Activate { source, .. } => Some(source.class()),
            Self::Cancelled(_) | Self::DeadlineExceeded => None,
        }
    }

    pub fn index(&self) -> Option<usize> {
        match self {
            Self::Prepare { index, .. } | Self::Activate { index, .. } => Some(*index),
            Self::Cancelled(_) | Self::DeadlineExceeded => None,
        }
    }
}

/// 按“全部准备成功 → 全部激活”的顺序绑定，失败时不返回半成品。
pub async fn bind_prepared(
    prepared: PreparedRuntime,
    factory: &dyn SocketFactory,
    deadline: Deadline,
    cancellation: &Cancellation,
) -> Result<BoundCandidate, BindError> {
    let plan = prepared.bind_plan().clone();
    let mut pending = Vec::with_capacity(plan.entries.len());

    for (index, entry) in plan.entries.iter().enumerate() {
        check_budget(deadline, cancellation)?;
        let socket = factory
            .prepare(socket_spec(entry), deadline, cancellation)
            .await
            .map_err(|source| BindError::Prepare {
                index,
                owner: entry.owner.clone(),
                source,
            })?;
        pending.push((entry.clone(), socket));
    }

    check_budget(deadline, cancellation)?;
    let mut endpoints = Vec::with_capacity(pending.len());
    for (index, (entry, socket)) in pending.into_iter().enumerate() {
        let activated = socket.activate().map_err(|source| BindError::Activate {
            index,
            owner: entry.owner.clone(),
            source,
        })?;
        endpoints.push(BoundEndpoint {
            entry,
            socket: activated,
        });
    }

    Ok(BoundCandidate {
        prepared,
        listeners: BoundListenerSet::new(endpoints),
    })
}

fn check_budget(deadline: Deadline, cancellation: &Cancellation) -> Result<(), BindError> {
    if cancellation.is_cancelled() {
        return Err(BindError::Cancelled(
            cancellation.reason().unwrap_or(CancelReason::Shutdown),
        ));
    }
    if deadline.is_expired(std::time::Instant::now()) {
        return Err(BindError::DeadlineExceeded);
    }
    Ok(())
}

fn socket_spec(entry: &BindEntry) -> SocketSpec {
    SocketSpec {
        kind: match entry.protocol {
            BindProtocol::Udp => SocketKind::Udp,
            BindProtocol::Tcp => SocketKind::Tcp,
        },
        address: SocketAddr::new(entry.address, entry.port),
        reuse_port: false,
        v6_only: entry.v6_only,
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use crate::config::{ConfigLoader, LoadOptions};
    use crate::dns::{CancelReason, Cancellation, Deadline, RuntimeRevision};
    use crate::ports::effects::{
        ActivatedSocket, PreparedSocket, SocketFactory, SocketKind, SocketSpec,
    };
    use crate::ports::{PortError, PortErrorClass, PortFuture};

    use super::{BindError, bind_prepared};
    use crate::runtime::PreparedRuntime;

    #[derive(Clone, Default)]
    struct FakeSocketState {
        events: Arc<Mutex<Vec<String>>>,
        specs: Arc<Mutex<Vec<SocketSpec>>>,
        prepared_drops: Arc<Mutex<usize>>,
        activated_drops: Arc<Mutex<usize>>,
    }

    impl FakeSocketState {
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn specs(&self) -> Vec<SocketSpec> {
            self.specs.lock().unwrap().clone()
        }

        fn prepared_drops(&self) -> usize {
            *self.prepared_drops.lock().unwrap()
        }

        fn activated_drops(&self) -> usize {
            *self.activated_drops.lock().unwrap()
        }
    }

    struct FakeFactory {
        state: FakeSocketState,
        prepare_failure_at: Option<usize>,
        activate_failure_at: Option<usize>,
        prepare_count: Mutex<usize>,
    }

    impl FakeFactory {
        fn new(state: FakeSocketState) -> Self {
            Self {
                state,
                prepare_failure_at: None,
                activate_failure_at: None,
                prepare_count: Mutex::new(0),
            }
        }
    }

    struct FakePreparedSocket {
        state: FakeSocketState,
        index: usize,
        spec: SocketSpec,
        activate_failure: bool,
    }

    impl Drop for FakePreparedSocket {
        fn drop(&mut self) {
            *self.state.prepared_drops.lock().unwrap() += 1;
        }
    }

    struct FakeActivatedSocket {
        state: FakeSocketState,
        address: SocketAddr,
    }

    impl Drop for FakeActivatedSocket {
        fn drop(&mut self) {
            *self.state.activated_drops.lock().unwrap() += 1;
        }
    }

    impl PreparedSocket for FakePreparedSocket {
        fn local_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(self.spec.address)
        }

        fn activate(self: Box<Self>) -> Result<Box<dyn ActivatedSocket>, PortError> {
            self.state
                .events
                .lock()
                .unwrap()
                .push(format!("activate-{}", self.index));
            if self.activate_failure {
                return Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "fake_socket.activate",
                ));
            }
            Ok(Box::new(FakeActivatedSocket {
                state: self.state.clone(),
                address: self.spec.address,
            }))
        }
    }

    impl ActivatedSocket for FakeActivatedSocket {
        fn local_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(self.address)
        }
    }

    impl SocketFactory for FakeFactory {
        fn prepare<'a>(
            &'a self,
            spec: SocketSpec,
            _deadline: Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<Box<dyn PreparedSocket>, PortError>> {
            let index = {
                let mut count = self.prepare_count.lock().unwrap();
                let index = *count;
                *count += 1;
                index
            };
            self.state
                .events
                .lock()
                .unwrap()
                .push(format!("prepare-{index}"));
            self.state.specs.lock().unwrap().push(spec);
            let failure = self.prepare_failure_at == Some(index);
            let activate_failure = self.activate_failure_at == Some(index);
            let state = self.state.clone();
            Box::pin(async move {
                if failure {
                    return Err(PortError::new(
                        PortErrorClass::Unavailable,
                        "fake_socket.prepare",
                    ));
                }
                Ok(Box::new(FakePreparedSocket {
                    state,
                    index,
                    spec,
                    activate_failure,
                }) as Box<dyn PreparedSocket>)
            })
        }
    }

    fn prepared_fixture() -> PreparedRuntime {
        let config = ConfigLoader::new(LoadOptions::default().without_snapshot())
            .load_str(
                r#"
version: 1
work:
  path: /tmp/fluxdns-runtime-bind-test
  rules_path: ./rules
database:
  type: sqlite
  path: ./data.sqlite
logs:
  enable: false
  level: info
  path: ./fluxdns.log
webui:
  enable: false
  address: 127.0.0.1
  port: 8080
  users: []
dns: {}
listener:
  - type: udp
    name: dns-udp
    addresses: [127.0.0.1]
    port: 5300
    strategy: default
  - type: tcp
    name: dns-tcp
    addresses: ["::1"]
    port: 5301
    strategy: default
upstreams:
  - type: hosts
    name: local
    format: hosts
    hosts: "127.0.0.1 example.test"
hosts:
  - type: const
    name: local-hosts
    format: hosts
    hosts: "127.0.0.1 example.test"
strategy:
  - name: default
    rules:
      - hosts: local-hosts
    default_upstream: local
"#,
            )
            .expect("bind fixture must be valid")
            .resolved;
        PreparedRuntime::prepare(config, RuntimeRevision(3)).unwrap()
    }

    fn budget() -> (Deadline, Cancellation) {
        (
            Deadline::new(Instant::now() + Duration::from_secs(30)),
            Cancellation::new(),
        )
    }

    #[tokio::test]
    async fn bind_prepares_every_endpoint_before_activation() {
        let state = FakeSocketState::default();
        let factory = FakeFactory::new(state.clone());
        let (deadline, cancellation) = budget();

        let candidate = bind_prepared(prepared_fixture(), &factory, deadline, &cancellation)
            .await
            .unwrap();

        assert_eq!(candidate.listeners().len(), 2);
        assert_eq!(
            state.events(),
            vec![
                "prepare-0".to_owned(),
                "prepare-1".to_owned(),
                "activate-0".to_owned(),
                "activate-1".to_owned()
            ]
        );
        let specs = state.specs();
        assert_eq!(specs[0].kind, SocketKind::Udp);
        assert_eq!(specs[0].address, SocketAddr::from(([127, 0, 0, 1], 5300)));
        assert!(!specs[0].v6_only);
        assert_eq!(specs[1].kind, SocketKind::Tcp);
        assert_eq!(
            specs[1].address,
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 5301))
        );
        assert!(specs[1].v6_only);
        assert_eq!(
            candidate.listeners().local_addrs().unwrap(),
            vec![specs[0].address, specs[1].address]
        );
    }

    #[tokio::test]
    async fn prepare_failure_drops_pending_sockets_without_activation() {
        let state = FakeSocketState::default();
        let mut factory = FakeFactory::new(state.clone());
        factory.prepare_failure_at = Some(1);
        let (deadline, cancellation) = budget();

        let error = bind_prepared(prepared_fixture(), &factory, deadline, &cancellation)
            .await
            .unwrap_err();

        assert_eq!(error.index(), Some(1));
        assert!(matches!(error, BindError::Prepare { .. }));
        assert!(
            state
                .events()
                .iter()
                .all(|event| !event.starts_with("activate"))
        );
        assert_eq!(state.prepared_drops(), 1);
        assert_eq!(state.activated_drops(), 0);
    }

    #[tokio::test]
    async fn activation_failure_drops_already_activated_and_remaining_sockets() {
        let state = FakeSocketState::default();
        let mut factory = FakeFactory::new(state.clone());
        factory.activate_failure_at = Some(1);
        let (deadline, cancellation) = budget();

        let error = bind_prepared(prepared_fixture(), &factory, deadline, &cancellation)
            .await
            .unwrap_err();

        assert_eq!(error.index(), Some(1));
        assert!(matches!(error, BindError::Activate { .. }));
        assert_eq!(state.prepared_drops(), 2);
        assert_eq!(state.activated_drops(), 1);
    }

    #[tokio::test]
    async fn cancellation_prevents_any_socket_preparation() {
        let state = FakeSocketState::default();
        let factory = FakeFactory::new(state.clone());
        let (deadline, cancellation) = budget();
        cancellation.cancel(CancelReason::Shutdown);

        let error = bind_prepared(prepared_fixture(), &factory, deadline, &cancellation)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            BindError::Cancelled(CancelReason::Shutdown)
        ));
        assert!(state.events().is_empty());
    }
}
