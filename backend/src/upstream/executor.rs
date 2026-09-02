//! Upstream group 的受监督执行编排。

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dns::{CancelReason, CanonicalQuery, RequestContext};
use crate::ports::PortFuture;
use crate::ports::exchange::{ConnectorId, DnsExchange, SelectionPolicy, UpstreamOutcome};

use super::outcome::aggregate_with_connector;
use super::{FallbackDecision, GroupSelector, GroupSelectorError, UpstreamAttempt, assess};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutorBuildError {
    EmptyExchanges,
    InvalidTimeout,
    InvalidMemberId { index: usize },
    DuplicateConnector { connector: String },
    MissingConnector { connector: String },
    UnexpectedConnector { connector: String },
}

impl fmt::Display for ExecutorBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExchanges => formatter.write_str("upstream group has no exchanges"),
            Self::InvalidTimeout => formatter.write_str("upstream group timeout must be positive"),
            Self::InvalidMemberId { index } => write!(
                formatter,
                "upstream group member {index} has an invalid connector id"
            ),
            Self::DuplicateConnector { connector } => {
                write!(formatter, "connector `{connector}` is duplicated")
            }
            Self::MissingConnector { connector } => {
                write!(formatter, "connector `{connector}` is missing")
            }
            Self::UnexpectedConnector { connector } => {
                write!(formatter, "connector `{connector}` is not a group member")
            }
        }
    }
}

impl std::error::Error for ExecutorBuildError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutorError {
    Selector(GroupSelectorError),
    Aggregate(super::OutcomeError),
    TaskFailed,
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Selector(error) => error.fmt(formatter),
            Self::Aggregate(error) => error.fmt(formatter),
            Self::TaskFailed => formatter.write_str("upstream exchange task failed"),
        }
    }
}

impl std::error::Error for ExecutorError {}

impl From<GroupSelectorError> for ExecutorError {
    fn from(error: GroupSelectorError) -> Self {
        Self::Selector(error)
    }
}

impl From<super::OutcomeError> for ExecutorError {
    fn from(error: super::OutcomeError) -> Self {
        Self::Aggregate(error)
    }
}

/// 接收 parallel 快速终态之后完成的 late attempt。
///
/// sink 只拥有后台结果，不拥有客户端响应；实现方应自行提供容量和关闭语义。
pub trait LateResultSink: Send + Sync {
    fn submit(&self, query: CanonicalQuery, context: RequestContext, attempt: UpstreamAttempt);

    /// 接管 parallel 快速返回后仍在执行的 attempt drain。
    ///
    /// 生产实现应把 future 放入自身的有界生命周期容器，避免 upstream 编排创建
    /// 无法在 shutdown 时回收的 detached task。
    fn spawn_drain(&self, task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>);
}

/// group 执行终态及实际产生该终态的成员。
pub(crate) struct GroupExecutionResult {
    pub(crate) connector: Option<ConnectorId>,
    pub(crate) outcome: UpstreamOutcome,
}

/// group 成员的已绑定执行句柄。
///
/// 嵌套 group 不能退化为普通 `DnsExchange`，否则外层 parallel 的 late sink
/// 会在 `GroupExchange` 边界丢失。该类型只在 upstream 编排层传递 sink。
#[derive(Clone)]
pub(crate) enum GroupMember {
    Direct(Arc<dyn DnsExchange>),
    Nested {
        connector: ConnectorId,
        executor: Arc<UpstreamGroupExecutor>,
    },
}

/// 执行一个已经解析的 upstream group。
pub struct UpstreamGroupExecutor {
    primary: ExecutionPhase,
    fallback: Option<ExecutionPhase>,
}

struct ExecutionPhase {
    selector: Arc<GroupSelector>,
    exchanges: Arc<[GroupMember]>,
    timeout: Option<Duration>,
}

impl GroupMember {
    fn connector_id(&self) -> &ConnectorId {
        match self {
            Self::Direct(exchange) => exchange.connector_id(),
            Self::Nested { connector, .. } => connector,
        }
    }

    fn exchange<'a>(
        &'a self,
        query: &'a CanonicalQuery,
        context: &'a RequestContext,
        late_sink: Option<Arc<dyn LateResultSink>>,
    ) -> PortFuture<'a, UpstreamOutcome> {
        match self {
            Self::Direct(exchange) => exchange.exchange(query, context),
            Self::Nested {
                connector,
                executor,
            } => {
                let connector = connector.clone();
                Box::pin(async move {
                    let result = match late_sink {
                        Some(sink) => executor.execute_with_late_sink(query, context, sink).await,
                        None => executor.execute(query, context).await,
                    };
                    result.unwrap_or(UpstreamOutcome::TransportFailure(
                        crate::ports::exchange::TransportFailure {
                            connector,
                            class: crate::ports::exchange::TransportFailureClass::Internal,
                            retryable: false,
                            safe_context: Some("nested group execution failed"),
                        },
                    ))
                })
            }
        }
    }
}

impl fmt::Debug for UpstreamGroupExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamGroupExecutor")
            .field("mode", &self.primary.selector.mode())
            .field("member_count", &self.primary.exchanges.len())
            .field("has_fallback", &self.fallback.is_some())
            .finish()
    }
}

impl UpstreamGroupExecutor {
    /// 按 selector 的配置顺序绑定 connector，拒绝缺失、重复和额外 connector。
    pub fn new(
        selector: GroupSelector,
        exchanges: Vec<Arc<dyn DnsExchange>>,
    ) -> Result<Self, ExecutorBuildError> {
        Self::build(selector, exchanges, None, None)
    }

    /// 构造一个带 primary group timeout 的执行器。
    pub fn new_with_timeout(
        selector: GroupSelector,
        exchanges: Vec<Arc<dyn DnsExchange>>,
        timeout: Duration,
    ) -> Result<Self, ExecutorBuildError> {
        Self::build(selector, exchanges, Some(timeout), None)
    }

    /// 构造带独立 fallback window 的 group 执行器。
    pub fn new_with_fallback(
        selector: GroupSelector,
        exchanges: Vec<Arc<dyn DnsExchange>>,
        timeout: Duration,
        fallback_selector: GroupSelector,
        fallback_exchanges: Vec<Arc<dyn DnsExchange>>,
        fallback_timeout: Duration,
    ) -> Result<Self, ExecutorBuildError> {
        let fallback = ExecutionPhase::new(
            fallback_selector,
            fallback_exchanges,
            Some(fallback_timeout),
        )?;
        Self::build(selector, exchanges, Some(timeout), Some(fallback))
    }

    pub(crate) fn new_with_members(
        selector: GroupSelector,
        members: Vec<GroupMember>,
        timeout: Duration,
    ) -> Result<Self, ExecutorBuildError> {
        Self::build_members(selector, members, Some(timeout), None)
    }

    pub(crate) fn new_with_fallback_members(
        selector: GroupSelector,
        members: Vec<GroupMember>,
        timeout: Duration,
        fallback_selector: GroupSelector,
        fallback_members: Vec<GroupMember>,
        fallback_timeout: Duration,
    ) -> Result<Self, ExecutorBuildError> {
        let fallback = ExecutionPhase::new_members(
            fallback_selector,
            fallback_members,
            Some(fallback_timeout),
        )?;
        Self::build_members(selector, members, Some(timeout), Some(fallback))
    }

    fn build(
        selector: GroupSelector,
        exchanges: Vec<Arc<dyn DnsExchange>>,
        timeout: Option<Duration>,
        fallback: Option<ExecutionPhase>,
    ) -> Result<Self, ExecutorBuildError> {
        let primary = ExecutionPhase::new(selector, exchanges, timeout)?;
        Ok(Self { primary, fallback })
    }

    fn build_members(
        selector: GroupSelector,
        members: Vec<GroupMember>,
        timeout: Option<Duration>,
        fallback: Option<ExecutionPhase>,
    ) -> Result<Self, ExecutorBuildError> {
        let primary = ExecutionPhase::new_members(selector, members, timeout)?;
        Ok(Self { primary, fallback })
    }

    pub fn selector(&self) -> &GroupSelector {
        &self.primary.selector
    }

    pub async fn execute(
        &self,
        query: &CanonicalQuery,
        context: &RequestContext,
    ) -> Result<UpstreamOutcome, ExecutorError> {
        Ok(self.execute_inner(query, context, None).await?.outcome)
    }

    pub async fn execute_with_late_sink(
        &self,
        query: &CanonicalQuery,
        context: &RequestContext,
        sink: Arc<dyn LateResultSink>,
    ) -> Result<UpstreamOutcome, ExecutorError> {
        Ok(self
            .execute_inner(query, context, Some(sink))
            .await?
            .outcome)
    }

    /// 执行 group 并返回实际选中的成员 ID。
    pub(crate) async fn execute_with_selection(
        &self,
        query: &CanonicalQuery,
        context: &RequestContext,
    ) -> Result<GroupExecutionResult, ExecutorError> {
        self.execute_inner(query, context, None).await
    }

    /// 执行带 late sink 的 group，并返回实际选中的成员 ID。
    pub(crate) async fn execute_with_selection_and_late_sink(
        &self,
        query: &CanonicalQuery,
        context: &RequestContext,
        sink: Arc<dyn LateResultSink>,
    ) -> Result<GroupExecutionResult, ExecutorError> {
        self.execute_inner(query, context, Some(sink)).await
    }

    async fn execute_inner(
        &self,
        query: &CanonicalQuery,
        context: &RequestContext,
        late_sink: Option<Arc<dyn LateResultSink>>,
    ) -> Result<GroupExecutionResult, ExecutorError> {
        let primary = self
            .execute_phase(&self.primary, query, context, late_sink.clone())
            .await?;
        if !primary.enter_fallback
            || self.fallback.is_none()
            || context.meta.cancellation.is_cancelled()
            || context.meta.deadline.is_expired(Instant::now())
        {
            return Ok(primary.result);
        }
        Ok(self
            .execute_phase(
                self.fallback.as_ref().expect("checked above"),
                query,
                context,
                late_sink,
            )
            .await?
            .result)
    }

    async fn execute_phase(
        &self,
        phase: &ExecutionPhase,
        query: &CanonicalQuery,
        context: &RequestContext,
        late_sink: Option<Arc<dyn LateResultSink>>,
    ) -> Result<PhaseResult, ExecutorError> {
        let phase_context = phase_context(context, phase.timeout);
        let attempts = if phase.selector.mode() == SelectionPolicy::Parallel {
            self.execute_parallel(phase, query, &phase_context, late_sink)
                .await?
        } else {
            self.execute_ordered(phase, query, &phase_context, late_sink)
                .await?
        };
        let enter_fallback = super::should_enter_fallback(&attempts);
        let result = aggregate_with_connector(phase.selector.mode(), query, attempts)?;
        Ok(PhaseResult {
            enter_fallback,
            result: GroupExecutionResult {
                connector: result.connector,
                outcome: result.outcome,
            },
        })
    }

    async fn execute_ordered(
        &self,
        phase: &ExecutionPhase,
        query: &CanonicalQuery,
        context: &RequestContext,
        late_sink: Option<Arc<dyn LateResultSink>>,
    ) -> Result<Vec<UpstreamAttempt>, ExecutorError> {
        let primary = match phase.selector.mode() {
            SelectionPolicy::LoadBalance => {
                let lease = phase.selector.acquire_primary()?;
                let result = self
                    .execute_ordered_members(phase, query, context, lease.member_index(), late_sink)
                    .await;
                drop(lease);
                return result;
            }
            SelectionPolicy::RoundRobin | SelectionPolicy::Failover => {
                phase.selector.select_primary()?
            }
            SelectionPolicy::Sequential | SelectionPolicy::Parallel => unreachable!(),
        };
        self.execute_ordered_members(phase, query, context, primary, late_sink)
            .await
    }

    async fn execute_ordered_members(
        &self,
        phase: &ExecutionPhase,
        query: &CanonicalQuery,
        context: &RequestContext,
        primary: usize,
        late_sink: Option<Arc<dyn LateResultSink>>,
    ) -> Result<Vec<UpstreamAttempt>, ExecutorError> {
        let mut indices = Vec::with_capacity(phase.exchanges.len());
        indices.push(primary);
        indices.extend(phase.selector.ordered_candidates(primary)?);
        let mut attempts = Vec::with_capacity(indices.len());
        for (attempt_index, index) in indices.into_iter().enumerate() {
            let exchange = &phase.exchanges[index];
            let outcome = match cancelled_outcome(context) {
                Some(outcome) => outcome,
                None => run_exchange(exchange, query, context, late_sink.clone()).await,
            };
            let should_stop = matches!(assess(&outcome).fallback, FallbackDecision::Stop);
            attempts.push(UpstreamAttempt {
                attempt_index,
                connector: exchange.connector_id().clone(),
                outcome,
            });
            if should_stop || context.meta.deadline.is_expired(Instant::now()) {
                break;
            }
        }
        Ok(attempts)
    }

    async fn execute_parallel(
        &self,
        phase: &ExecutionPhase,
        query: &CanonicalQuery,
        context: &RequestContext,
        late_sink: Option<Arc<dyn LateResultSink>>,
    ) -> Result<Vec<UpstreamAttempt>, ExecutorError> {
        let mut tasks = tokio::task::JoinSet::new();
        for index in phase.selector.parallel_order() {
            if let Some(outcome) = cancelled_outcome(context) {
                tasks.abort_all();
                return Ok(vec![UpstreamAttempt {
                    attempt_index: index,
                    connector: phase.exchanges[index].connector_id().clone(),
                    outcome,
                }]);
            }
            let exchange = phase.exchanges[index].clone();
            let query = query.clone();
            let context = context.clone();
            let late_sink = late_sink.clone();
            tasks.spawn(async move {
                let connector = exchange.connector_id().clone();
                let outcome = run_exchange(&exchange, &query, &context, late_sink).await;
                (index, connector, outcome)
            });
        }
        let mut attempts = Vec::with_capacity(phase.exchanges.len());
        while !tasks.is_empty() {
            let joined = tokio::select! {
                biased;
                _ = context.meta.cancellation.cancelled() => {
                    tasks.abort_all();
                    return Ok(vec![UpstreamAttempt {
                        attempt_index: phase.exchanges.len(),
                        connector: phase.exchanges[0].connector_id().clone(),
                        outcome: cancelled_outcome(context).expect("cancellation branch is cancelled"),
                    }]);
                }
                joined = tasks.join_next() => joined,
            };
            let Some(joined) = joined else {
                break;
            };
            let (index, connector, outcome) = match joined {
                Ok(result) => result,
                Err(_) => return Err(ExecutorError::TaskFailed),
            };
            let complete_response = matches!(
                &outcome,
                UpstreamOutcome::Response(response)
                    if response.class() == crate::dns::ResponseClass::Positive
            );
            let terminal_response = matches!(&outcome, UpstreamOutcome::Response(_))
                && matches!(assess(&outcome).fallback, FallbackDecision::Stop);
            attempts.push(UpstreamAttempt {
                attempt_index: index,
                connector,
                outcome,
            });
            if complete_response || (terminal_response && late_sink.is_some()) {
                if let Some(sink) = late_sink {
                    let drain = late_attempt_drain(
                        tasks,
                        query.clone(),
                        context.clone(),
                        Arc::clone(&sink),
                    );
                    sink.spawn_drain(drain);
                } else {
                    tasks.abort_all();
                }
                break;
            }
        }
        Ok(attempts)
    }
}

fn late_attempt_drain(
    mut tasks: tokio::task::JoinSet<(usize, ConnectorId, UpstreamOutcome)>,
    query: CanonicalQuery,
    context: RequestContext,
    sink: Arc<dyn LateResultSink>,
) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
    Box::pin(async move {
        while let Some(joined) = tasks.join_next().await {
            let Ok((attempt_index, connector, outcome)) = joined else {
                break;
            };
            if !matches!(outcome, UpstreamOutcome::Response(_)) {
                continue;
            }
            sink.submit(
                query.clone(),
                context.clone(),
                UpstreamAttempt {
                    attempt_index,
                    connector,
                    outcome,
                },
            );
        }
    })
}

impl ExecutionPhase {
    fn new(
        selector: GroupSelector,
        exchanges: Vec<Arc<dyn DnsExchange>>,
        timeout: Option<Duration>,
    ) -> Result<Self, ExecutorBuildError> {
        Self::new_members(
            selector,
            exchanges.into_iter().map(GroupMember::Direct).collect(),
            timeout,
        )
    }

    fn new_members(
        selector: GroupSelector,
        members: Vec<GroupMember>,
        timeout: Option<Duration>,
    ) -> Result<Self, ExecutorBuildError> {
        if members.is_empty() {
            return Err(ExecutorBuildError::EmptyExchanges);
        }
        if timeout.is_some_and(|value| value.is_zero()) {
            return Err(ExecutorBuildError::InvalidTimeout);
        }
        let mut by_id = HashMap::with_capacity(members.len());
        for member in members {
            let (id, exchange) = match member {
                GroupMember::Direct(exchange) => (
                    exchange.connector_id().clone(),
                    GroupMember::Direct(exchange),
                ),
                GroupMember::Nested {
                    connector,
                    executor,
                } => (
                    connector.clone(),
                    GroupMember::Nested {
                        connector,
                        executor,
                    },
                ),
            };
            if by_id.insert(id.clone(), exchange).is_some() {
                return Err(ExecutorBuildError::DuplicateConnector {
                    connector: id.as_str().to_owned(),
                });
            }
        }
        let mut ordered = Vec::with_capacity(selector.members().len());
        for (index, member) in selector.members().iter().enumerate() {
            let id = ConnectorId::new(member.name.as_str().to_owned())
                .map_err(|_| ExecutorBuildError::InvalidMemberId { index })?;
            let exchange =
                by_id
                    .remove(&id)
                    .ok_or_else(|| ExecutorBuildError::MissingConnector {
                        connector: id.as_str().to_owned(),
                    })?;
            ordered.push(exchange);
        }
        if let Some((id, _)) = by_id.into_iter().next() {
            return Err(ExecutorBuildError::UnexpectedConnector {
                connector: id.as_str().to_owned(),
            });
        }
        Ok(Self {
            selector: Arc::new(selector),
            exchanges: ordered.into(),
            timeout,
        })
    }
}

struct PhaseResult {
    result: GroupExecutionResult,
    enter_fallback: bool,
}

fn phase_context(context: &RequestContext, timeout: Option<Duration>) -> RequestContext {
    let mut phase_context = context.clone();
    if let Some(timeout) = timeout
        && let Some(deadline) = Instant::now().checked_add(timeout)
    {
        phase_context.meta.deadline = phase_context.meta.deadline.shortened_to(deadline);
    }
    phase_context
}

async fn run_exchange(
    exchange: &GroupMember,
    query: &CanonicalQuery,
    context: &RequestContext,
    late_sink: Option<Arc<dyn LateResultSink>>,
) -> UpstreamOutcome {
    if let Some(outcome) = cancelled_outcome(context) {
        return outcome;
    }
    let remaining = context.meta.deadline.remaining(Instant::now());
    if remaining.is_zero() {
        return timeout_failure(exchange);
    }
    tokio::select! {
        biased;
        _ = context.meta.cancellation.cancelled() => UpstreamOutcome::Cancelled(
            context.meta.cancellation.reason().unwrap_or(CancelReason::UpstreamCancelled),
        ),
        _ = tokio::time::sleep(remaining) => timeout_failure(exchange),
        outcome = exchange.exchange(query, context, late_sink) => outcome,
    }
}

fn timeout_failure(exchange: &GroupMember) -> UpstreamOutcome {
    UpstreamOutcome::TransportFailure(crate::ports::exchange::TransportFailure {
        connector: exchange.connector_id().clone(),
        class: crate::ports::exchange::TransportFailureClass::Timeout,
        retryable: true,
        safe_context: Some("upstream group deadline"),
    })
}

fn cancelled_outcome(context: &RequestContext) -> Option<UpstreamOutcome> {
    context.meta.cancellation.is_cancelled().then(|| {
        UpstreamOutcome::Cancelled(
            context
                .meta
                .cancellation
                .reason()
                .unwrap_or(CancelReason::UpstreamCancelled),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RData, Record, RecordType, rdata::A};

    use super::{ExecutorBuildError, GroupMember, LateResultSink, UpstreamGroupExecutor};
    use crate::config::resolve::{ConfigId, ResolvedUpstreamMember};
    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalQuery, CanonicalResponse, ClientIdentity,
        Deadline, ListenerId, RequestContext, RequestId, RequestMeta, ResponseClass,
        RuntimeRevision, TransportCapabilities, TransportClass,
    };
    use crate::ports::PortFuture;
    use crate::ports::exchange::{
        ConnectorId, DnsExchange, SelectionPolicy, TransportFailure, TransportFailureClass,
        UpstreamOutcome,
    };
    use crate::ports::testing::FakeExchange;

    fn query() -> CanonicalQuery {
        let mut message = Message::new(1, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        CanonicalQuery::from_message(message).unwrap()
    }

    fn context() -> RequestContext {
        RequestContext {
            meta: RequestMeta {
                request_id: RequestId(1),
                trace_id: None,
                received_at: Instant::now(),
                received_at_utc: SystemTime::now(),
                deadline: Deadline::new(Instant::now() + Duration::from_secs(30)),
                cancellation: Cancellation::new(),
                connection_id: None,
                stream_id: None,
                listener_id: ListenerId::from("test"),
                route_id: None,
                original_dns_id: Some(1),
            },
            client: ClientIdentity::default(),
            transport: TransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            },
            runtime_revision: RuntimeRevision(1),
        }
    }

    fn member(name: &str) -> ResolvedUpstreamMember {
        ResolvedUpstreamMember {
            name: ConfigId::new(name).unwrap(),
            weight: 1,
        }
    }

    fn exchange(name: &str) -> Arc<FakeExchange> {
        Arc::new(FakeExchange::new(ConnectorId::new(name).unwrap()))
    }

    fn response(query: &CanonicalQuery, code: ResponseCode) -> UpstreamOutcome {
        UpstreamOutcome::Response(CanonicalResponse::empty_response(query, code).unwrap())
    }

    fn positive_response(query: &CanonicalQuery) -> UpstreamOutcome {
        UpstreamOutcome::Response(
            CanonicalResponse::response_with_code(
                query,
                ResponseCode::NoError,
                [Record::from_rdata(
                    query.question().name().clone(),
                    30,
                    RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, 1))),
                )],
            )
            .unwrap(),
        )
    }

    fn timeout(exchange: &FakeExchange, retryable: bool) -> UpstreamOutcome {
        UpstreamOutcome::TransportFailure(TransportFailure {
            connector: exchange.connector_id().clone(),
            class: TransportFailureClass::Timeout,
            retryable,
            safe_context: Some("test"),
        })
    }

    struct DeadlineExchange {
        connector: ConnectorId,
        deadline: Mutex<Option<Instant>>,
        outcome: Mutex<Option<UpstreamOutcome>>,
    }

    impl DeadlineExchange {
        fn new(connector: ConnectorId, outcome: UpstreamOutcome) -> Self {
            Self {
                connector,
                deadline: Mutex::new(None),
                outcome: Mutex::new(Some(outcome)),
            }
        }

        fn deadline(&self) -> Option<Instant> {
            *self.deadline.lock().unwrap()
        }
    }

    struct DelayedExchange {
        connector: ConnectorId,
        delay: Duration,
        outcome: Mutex<Option<UpstreamOutcome>>,
    }

    #[derive(Default)]
    struct LateCollector {
        results: Mutex<Vec<(usize, String, ResponseClass)>>,
    }

    impl LateCollector {
        fn results(&self) -> Vec<(usize, String, ResponseClass)> {
            self.results.lock().unwrap().clone()
        }
    }

    impl LateResultSink for LateCollector {
        fn submit(
            &self,
            query: CanonicalQuery,
            _context: RequestContext,
            attempt: super::super::UpstreamAttempt,
        ) {
            let UpstreamOutcome::Response(response) = attempt.outcome else {
                return;
            };
            if !response.matches_query(&query) {
                return;
            }
            self.results.lock().unwrap().push((
                attempt.attempt_index,
                attempt.connector.as_str().to_owned(),
                response.class(),
            ));
        }

        fn spawn_drain(
            &self,
            task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
        ) {
            tokio::spawn(task);
        }
    }

    impl DelayedExchange {
        fn new(connector: ConnectorId, delay: Duration, outcome: UpstreamOutcome) -> Self {
            Self {
                connector,
                delay,
                outcome: Mutex::new(Some(outcome)),
            }
        }
    }

    impl DnsExchange for DelayedExchange {
        fn connector_id(&self) -> &ConnectorId {
            &self.connector
        }

        fn exchange<'a>(
            &'a self,
            _query: &'a CanonicalQuery,
            _context: &'a RequestContext,
        ) -> PortFuture<'a, UpstreamOutcome> {
            let delay = self.delay;
            let outcome = self.outcome.lock().unwrap().take();
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                outcome.expect("delayed exchange must be called once")
            })
        }
    }

    impl DnsExchange for DeadlineExchange {
        fn connector_id(&self) -> &ConnectorId {
            &self.connector
        }

        fn exchange<'a>(
            &'a self,
            _query: &'a CanonicalQuery,
            context: &'a RequestContext,
        ) -> PortFuture<'a, UpstreamOutcome> {
            *self.deadline.lock().unwrap() = Some(context.meta.deadline.at());
            let outcome = self.outcome.lock().unwrap().take().unwrap();
            Box::pin(async move { outcome })
        }
    }

    #[tokio::test]
    async fn failover_retries_transport_failure_in_configuration_order() {
        let query = query();
        let first = exchange("one");
        let second = exchange("two");
        first.push(timeout(&first, true)).unwrap();
        second
            .push(response(&query, ResponseCode::NoError))
            .unwrap();
        let selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Failover,
            vec![member("one"), member("two")],
        )
        .unwrap();
        let executor =
            UpstreamGroupExecutor::new(selector, vec![first.clone(), second.clone()]).unwrap();
        let result = executor.execute(&query, &context()).await.unwrap();
        assert!(matches!(result, UpstreamOutcome::Response(_)));
        assert_eq!(first.calls(), 1);
        assert_eq!(second.calls(), 1);
    }

    #[tokio::test]
    async fn round_robin_and_load_balance_are_deterministic() {
        let query = query();
        let one = exchange("one");
        let two = exchange("two");
        one.push(response(&query, ResponseCode::NoError)).unwrap();
        two.push(response(&query, ResponseCode::NXDomain)).unwrap();
        let selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::RoundRobin,
            vec![member("one"), member("two")],
        )
        .unwrap();
        let executor =
            UpstreamGroupExecutor::new(selector, vec![one.clone(), two.clone()]).unwrap();
        executor.execute(&query, &context()).await.unwrap();
        let second = executor.execute(&query, &context()).await.unwrap();
        assert_eq!(one.calls(), 1);
        assert_eq!(two.calls(), 1);
        assert!(
            matches!(second, UpstreamOutcome::Response(response) if response.class() == crate::dns::ResponseClass::NxDomain)
        );

        let selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::LoadBalance,
            vec![member("one"), member("two")],
        )
        .unwrap();
        let executor =
            UpstreamGroupExecutor::new(selector, vec![one.clone(), two.clone()]).unwrap();
        executor.execute(&query, &context()).await.unwrap();
        assert_eq!(executor.selector().in_flight(0).unwrap(), 0);
    }

    #[tokio::test]
    async fn parallel_aggregates_and_honors_cancellation() {
        let query = query();
        let one = exchange("one");
        let two = exchange("two");
        one.push(response(&query, ResponseCode::NoError)).unwrap();
        two.push(timeout(&two, true)).unwrap();
        let selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Parallel,
            vec![member("one"), member("two")],
        )
        .unwrap();
        let executor = UpstreamGroupExecutor::new(selector, vec![one, two]).unwrap();
        let result = executor.execute(&query, &context()).await.unwrap();
        assert!(matches!(result, UpstreamOutcome::Response(_)));
        let cancelled = context();
        cancelled
            .meta
            .cancellation
            .cancel(crate::dns::CancelReason::Shutdown);
        let result = executor.execute(&query, &cancelled).await.unwrap();
        assert!(matches!(
            result,
            UpstreamOutcome::Cancelled(crate::dns::CancelReason::Shutdown)
        ));
    }

    #[tokio::test]
    async fn parallel_returns_complete_response_without_waiting_for_late_attempt() {
        let query = query();
        let fast = exchange("fast");
        fast.push(positive_response(&query)).unwrap();
        let slow = Arc::new(DelayedExchange::new(
            ConnectorId::new("slow").unwrap(),
            Duration::from_millis(250),
            response(&query, ResponseCode::NXDomain),
        ));
        let selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Parallel,
            vec![member("fast"), member("slow")],
        )
        .unwrap();
        let executor = UpstreamGroupExecutor::new(selector, vec![fast, slow]).unwrap();
        let started = Instant::now();
        let result = executor.execute(&query, &context()).await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(matches!(
            result,
            UpstreamOutcome::Response(response) if response.class() == crate::dns::ResponseClass::Positive
        ));
    }

    #[tokio::test]
    async fn parallel_sends_late_attempt_to_sink_without_delaying_response() {
        let query = query();
        let fast = exchange("fast");
        fast.push(positive_response(&query)).unwrap();
        let slow = Arc::new(DelayedExchange::new(
            ConnectorId::new("slow").unwrap(),
            Duration::from_millis(25),
            response(&query, ResponseCode::NoError),
        ));
        let selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Parallel,
            vec![member("fast"), member("slow")],
        )
        .unwrap();
        let executor = UpstreamGroupExecutor::new(selector, vec![fast, slow]).unwrap();
        let sink = Arc::new(LateCollector::default());
        let started = Instant::now();
        let result = executor
            .execute_with_late_sink(&query, &context(), sink.clone())
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(matches!(
            result,
            UpstreamOutcome::Response(response) if response.class() == ResponseClass::Positive
        ));

        for _ in 0..100 {
            if !sink.results().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            sink.results(),
            vec![(1, "slow".to_owned(), ResponseClass::NoData)]
        );
    }

    #[tokio::test]
    async fn parallel_returns_terminal_nodata_while_draining_late_positive() {
        let query = query();
        let early = exchange("early");
        early.push(response(&query, ResponseCode::NoError)).unwrap();
        let late = Arc::new(DelayedExchange::new(
            ConnectorId::new("late").unwrap(),
            Duration::from_millis(250),
            positive_response(&query),
        ));
        let selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Parallel,
            vec![member("early"), member("late")],
        )
        .unwrap();
        let executor = UpstreamGroupExecutor::new(selector, vec![early, late]).unwrap();
        let sink = Arc::new(LateCollector::default());
        let started = Instant::now();
        let result = executor
            .execute_with_late_sink(&query, &context(), sink.clone())
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(200));
        assert!(matches!(
            result,
            UpstreamOutcome::Response(response) if response.class() == ResponseClass::NoData
        ));

        for _ in 0..300 {
            if sink.results().iter().any(|(_, connector, class)| {
                connector == "late" && *class == ResponseClass::Positive
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(sink.results().iter().any(|(_, connector, class)| {
            connector == "late" && *class == ResponseClass::Positive
        }));
    }

    #[tokio::test]
    async fn nested_parallel_group_propagates_late_attempt_to_outer_sink() {
        let query = query();
        let inner_fast = exchange("inner-fast");
        inner_fast.push(positive_response(&query)).unwrap();
        let inner_slow = Arc::new(DelayedExchange::new(
            ConnectorId::new("inner-slow").unwrap(),
            Duration::from_millis(25),
            response(&query, ResponseCode::NoError),
        ));
        let inner_selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Parallel,
            vec![member("inner-fast"), member("inner-slow")],
        )
        .unwrap();
        let inner = Arc::new(
            UpstreamGroupExecutor::new(inner_selector, vec![inner_fast, inner_slow]).unwrap(),
        );

        let outer_slow = Arc::new(DelayedExchange::new(
            ConnectorId::new("outer-slow").unwrap(),
            Duration::from_millis(100),
            response(&query, ResponseCode::NoError),
        ));
        let outer_selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Parallel,
            vec![member("inner"), member("outer-slow")],
        )
        .unwrap();
        let outer = UpstreamGroupExecutor::new_with_members(
            outer_selector,
            vec![
                GroupMember::Nested {
                    connector: ConnectorId::new("inner").unwrap(),
                    executor: inner,
                },
                GroupMember::Direct(outer_slow),
            ],
            Duration::from_secs(1),
        )
        .unwrap();
        let sink = Arc::new(LateCollector::default());
        let result = outer
            .execute_with_late_sink(&query, &context(), sink.clone())
            .await
            .unwrap();
        assert!(matches!(
            result,
            UpstreamOutcome::Response(response) if response.class() == ResponseClass::Positive
        ));

        for _ in 0..100 {
            if sink
                .results()
                .iter()
                .any(|(_, connector, _)| connector == "inner-slow")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(sink.results().iter().any(|(_, connector, class)| {
            connector == "inner-slow" && *class == ResponseClass::NoData
        }));
    }

    #[tokio::test]
    async fn parallel_late_window_prefers_positive_over_early_nodata() {
        let query = query();
        let early = exchange("early");
        early.push(response(&query, ResponseCode::NoError)).unwrap();
        let late = Arc::new(DelayedExchange::new(
            ConnectorId::new("late").unwrap(),
            Duration::from_millis(25),
            positive_response(&query),
        ));
        let selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Parallel,
            vec![member("early"), member("late")],
        )
        .unwrap();
        let executor = UpstreamGroupExecutor::new(selector, vec![early, late]).unwrap();
        let result = executor.execute(&query, &context()).await.unwrap();
        assert!(matches!(
            result,
            UpstreamOutcome::Response(response) if response.class() == crate::dns::ResponseClass::Positive
        ));
    }

    #[tokio::test]
    async fn fallback_runs_after_primary_retryable_failures() {
        let query = query();
        let primary = exchange("primary");
        let fallback = exchange("fallback");
        primary.push(timeout(&primary, true)).unwrap();
        fallback
            .push(response(&query, ResponseCode::NoError))
            .unwrap();
        let primary_selector =
            crate::upstream::GroupSelector::new(SelectionPolicy::Failover, vec![member("primary")])
                .unwrap();
        let fallback_selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Failover,
            vec![member("fallback")],
        )
        .unwrap();
        let executor = UpstreamGroupExecutor::new_with_fallback(
            primary_selector,
            vec![primary.clone()],
            Duration::from_secs(1),
            fallback_selector,
            vec![fallback.clone()],
            Duration::from_secs(1),
        )
        .unwrap();

        let result = executor.execute(&query, &context()).await.unwrap();
        assert!(matches!(
            result,
            UpstreamOutcome::Response(response) if response.class() == crate::dns::ResponseClass::NoData
        ));
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 1);
    }

    #[tokio::test]
    async fn terminal_primary_response_does_not_run_fallback() {
        let query = query();
        let primary = exchange("primary");
        let fallback = exchange("fallback");
        primary
            .push(response(&query, ResponseCode::NoError))
            .unwrap();
        fallback
            .push(response(&query, ResponseCode::NXDomain))
            .unwrap();
        let primary_selector =
            crate::upstream::GroupSelector::new(SelectionPolicy::Failover, vec![member("primary")])
                .unwrap();
        let fallback_selector = crate::upstream::GroupSelector::new(
            SelectionPolicy::Failover,
            vec![member("fallback")],
        )
        .unwrap();
        let executor = UpstreamGroupExecutor::new_with_fallback(
            primary_selector,
            vec![primary.clone()],
            Duration::from_secs(1),
            fallback_selector,
            vec![fallback.clone()],
            Duration::from_secs(1),
        )
        .unwrap();

        let result = executor.execute(&query, &context()).await.unwrap();
        assert!(matches!(
            result,
            UpstreamOutcome::Response(response) if response.class() == crate::dns::ResponseClass::NoData
        ));
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 0);
    }

    #[tokio::test]
    async fn group_timeout_shortens_phase_context() {
        let query = query();
        let exchange = Arc::new(DeadlineExchange::new(
            ConnectorId::new("one").unwrap(),
            response(&query, ResponseCode::NoError),
        ));
        let selector =
            crate::upstream::GroupSelector::new(SelectionPolicy::Failover, vec![member("one")])
                .unwrap();
        let context = context();
        let original = context.meta.deadline.at();
        let executor = UpstreamGroupExecutor::new_with_timeout(
            selector,
            vec![exchange.clone()],
            Duration::from_secs(1),
        )
        .unwrap();

        let result = executor.execute(&query, &context).await.unwrap();
        assert!(matches!(result, UpstreamOutcome::Response(_)));
        let phase_deadline = exchange.deadline().expect("exchange must observe deadline");
        assert!(phase_deadline < original);
    }

    #[test]
    fn zero_group_timeout_is_rejected() {
        let selector =
            crate::upstream::GroupSelector::new(SelectionPolicy::Failover, vec![member("one")])
                .unwrap();
        assert_eq!(
            UpstreamGroupExecutor::new_with_timeout(
                selector,
                vec![exchange("one")],
                Duration::ZERO,
            )
            .unwrap_err(),
            super::ExecutorBuildError::InvalidTimeout
        );
    }

    #[test]
    fn rejects_connector_set_that_does_not_match_group_members() {
        let selector =
            crate::upstream::GroupSelector::new(SelectionPolicy::Failover, vec![member("one")])
                .unwrap();
        let error = UpstreamGroupExecutor::new(selector, vec![exchange("two")]).unwrap_err();
        assert_eq!(
            error,
            ExecutorBuildError::MissingConnector {
                connector: "one".to_owned()
            }
        );
    }
}
