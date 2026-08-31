//! Upstream group 的受监督执行编排。

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::dns::{CancelReason, CanonicalQuery, RequestContext};
use crate::ports::exchange::{ConnectorId, DnsExchange, SelectionPolicy, UpstreamOutcome};

use super::{
    FallbackDecision, GroupSelector, GroupSelectorError, UpstreamAttempt, aggregate, assess,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutorBuildError {
    EmptyExchanges,
    InvalidMemberId { index: usize },
    DuplicateConnector { connector: String },
    MissingConnector { connector: String },
    UnexpectedConnector { connector: String },
}

impl fmt::Display for ExecutorBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExchanges => formatter.write_str("upstream group has no exchanges"),
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

/// 执行一个已经解析的 upstream group。
pub struct UpstreamGroupExecutor {
    selector: Arc<GroupSelector>,
    exchanges: Arc<[Arc<dyn DnsExchange>]>,
}

impl fmt::Debug for UpstreamGroupExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamGroupExecutor")
            .field("mode", &self.selector.mode())
            .field("member_count", &self.exchanges.len())
            .finish()
    }
}

impl UpstreamGroupExecutor {
    /// 按 selector 的配置顺序绑定 connector，拒绝缺失、重复和额外 connector。
    pub fn new(
        selector: GroupSelector,
        exchanges: Vec<Arc<dyn DnsExchange>>,
    ) -> Result<Self, ExecutorBuildError> {
        if exchanges.is_empty() {
            return Err(ExecutorBuildError::EmptyExchanges);
        }
        let mut by_id = HashMap::with_capacity(exchanges.len());
        for exchange in exchanges {
            let id = exchange.connector_id().clone();
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
        })
    }

    pub fn selector(&self) -> &GroupSelector {
        &self.selector
    }

    pub async fn execute(
        &self,
        query: &CanonicalQuery,
        context: &RequestContext,
    ) -> Result<UpstreamOutcome, ExecutorError> {
        if let Some(outcome) = cancelled_outcome(context) {
            return Ok(outcome);
        }
        if self.selector.mode() == SelectionPolicy::Parallel {
            return self.execute_parallel(query, context).await;
        }
        let primary = match self.selector.mode() {
            SelectionPolicy::LoadBalance => {
                let lease = self.selector.acquire_primary()?;
                let result = self
                    .execute_ordered(query, context, lease.member_index())
                    .await;
                drop(lease);
                return result;
            }
            SelectionPolicy::RoundRobin | SelectionPolicy::Failover => {
                self.selector.select_primary()?
            }
            SelectionPolicy::Sequential | SelectionPolicy::Parallel => unreachable!(),
        };
        self.execute_ordered(query, context, primary).await
    }

    async fn execute_ordered(
        &self,
        query: &CanonicalQuery,
        context: &RequestContext,
        primary: usize,
    ) -> Result<UpstreamOutcome, ExecutorError> {
        let mut indices = Vec::with_capacity(self.exchanges.len());
        indices.push(primary);
        indices.extend(self.selector.ordered_candidates(primary)?);
        let mut attempts = Vec::with_capacity(indices.len());
        for (attempt_index, index) in indices.into_iter().enumerate() {
            let exchange = &self.exchanges[index];
            let outcome = match cancelled_outcome(context) {
                Some(outcome) => outcome,
                None => tokio::select! {
                    outcome = exchange.exchange(query, context) => outcome,
                    _ = context.meta.cancellation.cancelled() => UpstreamOutcome::Cancelled(
                        context.meta.cancellation.reason().unwrap_or(CancelReason::UpstreamCancelled),
                    ),
                },
            };
            let should_stop = matches!(assess(&outcome).fallback, FallbackDecision::Stop);
            attempts.push(UpstreamAttempt {
                attempt_index,
                connector: exchange.connector_id().clone(),
                outcome,
            });
            if should_stop {
                break;
            }
        }
        Ok(aggregate(self.selector.mode(), query, attempts)?)
    }

    async fn execute_parallel(
        &self,
        query: &CanonicalQuery,
        context: &RequestContext,
    ) -> Result<UpstreamOutcome, ExecutorError> {
        let mut handles = Vec::with_capacity(self.exchanges.len());
        for index in self.selector.parallel_order() {
            if let Some(outcome) = cancelled_outcome(context) {
                abort_all(handles);
                return Ok(aggregate(
                    self.selector.mode(),
                    query,
                    vec![UpstreamAttempt {
                        attempt_index: index,
                        connector: self.exchanges[index].connector_id().clone(),
                        outcome,
                    }],
                )?);
            }
            let exchange = Arc::clone(&self.exchanges[index]);
            let query = query.clone();
            let context = context.clone();
            handles.push(tokio::spawn(async move {
                let outcome = tokio::select! {
                    outcome = exchange.exchange(&query, &context) => outcome,
                    _ = context.meta.cancellation.cancelled() => UpstreamOutcome::Cancelled(
                        context.meta.cancellation.reason().unwrap_or(CancelReason::UpstreamCancelled),
                    ),
                };
                (index, exchange.connector_id().clone(), outcome)
            }));
        }
        let mut pending = handles.into_iter();
        let mut attempts = Vec::with_capacity(self.exchanges.len());
        while let Some(handle) = pending.next() {
            let (index, connector, outcome) = match handle.await {
                Ok(result) => result,
                Err(_) => {
                    for handle in pending {
                        handle.abort();
                    }
                    return Err(ExecutorError::TaskFailed);
                }
            };
            attempts.push(UpstreamAttempt {
                attempt_index: index,
                connector,
                outcome,
            });
        }
        Ok(aggregate(self.selector.mode(), query, attempts)?)
    }
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

fn abort_all<T>(handles: Vec<tokio::task::JoinHandle<T>>) {
    for handle in handles {
        handle.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};

    use super::{ExecutorBuildError, UpstreamGroupExecutor};
    use crate::config::resolve::{ConfigId, ResolvedUpstreamMember};
    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalQuery, CanonicalResponse, ClientIdentity,
        Deadline, ListenerId, RequestContext, RequestId, RequestMeta, RuntimeRevision,
        TransportCapabilities, TransportClass,
    };
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

    fn timeout(exchange: &FakeExchange, retryable: bool) -> UpstreamOutcome {
        UpstreamOutcome::TransportFailure(TransportFailure {
            connector: exchange.connector_id().clone(),
            class: TransportFailureClass::Timeout,
            retryable,
            safe_context: Some("test"),
        })
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
