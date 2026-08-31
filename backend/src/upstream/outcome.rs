//! 无网络副作用的 upstream group 结果聚合。
//!
//! 聚合器只消费已经完成的尝试，因此并行请求的完成顺序不会改变结果：
//! `attempt_index` 始终代表配置顺序。Core 后续可以据此决定何时发起下一轮
//! fallback，或把一次并行批次交给这里生成最终结果。

use std::collections::HashSet;

use hickory_proto::op::ResponseCode;

use crate::dns::{CancelReason, CanonicalQuery, CanonicalResponse, ResponseClass};
use crate::ports::exchange::{
    ConnectorId, SelectionPolicy, TransportFailureClass, UpstreamOutcome,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptClass {
    Response(ResponseClass),
    TransportFailure {
        class: TransportFailureClass,
        retryable: bool,
    },
    Cancelled(CancelReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FallbackDecision {
    Continue,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptAssessment {
    pub class: AttemptClass,
    pub fallback: FallbackDecision,
}

pub struct UpstreamAttempt {
    pub attempt_index: usize,
    pub connector: ConnectorId,
    pub outcome: UpstreamOutcome,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutcomeError {
    DuplicateAttemptIndex { index: usize },
}

impl std::fmt::Display for OutcomeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateAttemptIndex { index } => {
                write!(formatter, "upstream attempt index {index} is duplicated")
            }
        }
    }
}

impl std::error::Error for OutcomeError {}

pub fn assess(outcome: &UpstreamOutcome) -> AttemptAssessment {
    match outcome {
        UpstreamOutcome::Response(response) => AttemptAssessment {
            class: AttemptClass::Response(response.class()),
            fallback: match response.class() {
                ResponseClass::ServFail | ResponseClass::Truncated => FallbackDecision::Continue,
                ResponseClass::Positive
                | ResponseClass::NoData
                | ResponseClass::NxDomain
                | ResponseClass::Refused
                | ResponseClass::Other(_) => FallbackDecision::Stop,
            },
        },
        UpstreamOutcome::TransportFailure(failure) => AttemptAssessment {
            class: AttemptClass::TransportFailure {
                class: failure.class,
                retryable: failure.retryable,
            },
            fallback: if failure.retryable {
                FallbackDecision::Continue
            } else {
                FallbackDecision::Stop
            },
        },
        UpstreamOutcome::Cancelled(reason) => AttemptAssessment {
            class: AttemptClass::Cancelled(*reason),
            fallback: FallbackDecision::Stop,
        },
    }
}

/// 按配置顺序合并 primary 和 fallback，并删除重复 connector。
pub fn deduplicate_connector_order(
    primary: impl IntoIterator<Item = ConnectorId>,
    fallback: impl IntoIterator<Item = ConnectorId>,
) -> Vec<ConnectorId> {
    let mut seen = HashSet::new();
    primary
        .into_iter()
        .chain(fallback)
        .filter(|connector| seen.insert(connector.clone()))
        .collect()
}

/// 聚合一次已经完成的 group 尝试。
///
/// `Failover` 及单主选择模式按 `attempt_index` 返回首个可接受 DNS response；
/// `Parallel` 同样按配置顺序择优，避免由 task 完成时序引入不确定性。SERVFAIL、
/// truncated response 和可重试 transport failure 会继续 fallback；若没有可接受
/// response，取消优先于固定 SERVFAIL。
pub fn aggregate(
    _mode: SelectionPolicy,
    query: &CanonicalQuery,
    mut attempts: Vec<UpstreamAttempt>,
) -> Result<UpstreamOutcome, OutcomeError> {
    attempts.sort_by_key(|attempt| attempt.attempt_index);
    for pair in attempts.windows(2) {
        if pair[0].attempt_index == pair[1].attempt_index {
            return Err(OutcomeError::DuplicateAttemptIndex {
                index: pair[0].attempt_index,
            });
        }
    }

    let mut cancellation = None;
    let mut selected_response = None;
    let mut fallback_blocked = false;
    for attempt in attempts {
        if fallback_blocked {
            if let UpstreamOutcome::Cancelled(reason) = attempt.outcome {
                cancellation = Some(prefer_cancel(cancellation, reason));
            }
            continue;
        }
        match assess(&attempt.outcome).fallback {
            FallbackDecision::Continue => {
                if let UpstreamOutcome::Cancelled(reason) = attempt.outcome {
                    cancellation = Some(prefer_cancel(cancellation, reason));
                }
            }
            FallbackDecision::Stop => match attempt.outcome {
                UpstreamOutcome::Response(candidate) => {
                    if selected_response.is_some() {
                        continue;
                    }
                    selected_response = Some(candidate);
                }
                UpstreamOutcome::Cancelled(reason) => {
                    cancellation = Some(prefer_cancel(cancellation, reason));
                }
                UpstreamOutcome::TransportFailure(failure) => {
                    fallback_blocked |= !failure.retryable;
                }
            },
        }
    }

    if let Some(reason) = cancellation.filter(|reason| cancel_priority(*reason) >= 3) {
        return Ok(UpstreamOutcome::Cancelled(reason));
    }
    if let Some(response) = selected_response {
        return Ok(UpstreamOutcome::Response(response));
    }
    if let Some(reason) = cancellation {
        return Ok(UpstreamOutcome::Cancelled(reason));
    }
    Ok(UpstreamOutcome::Response(
        CanonicalResponse::empty_response(query, ResponseCode::ServFail)
            .expect("canonical SERVFAIL response construction is infallible"),
    ))
}

fn prefer_cancel(current: Option<CancelReason>, candidate: CancelReason) -> CancelReason {
    current.map_or(candidate, |current| {
        if cancel_priority(candidate) > cancel_priority(current) {
            candidate
        } else {
            current
        }
    })
}

fn cancel_priority(reason: CancelReason) -> u8 {
    match reason {
        CancelReason::Shutdown => 5,
        CancelReason::ClientDisconnected => 4,
        CancelReason::DeadlineExceeded => 3,
        CancelReason::GroupPolicy => 2,
        CancelReason::UpstreamCancelled => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};

    use crate::dns::{CancelReason, CanonicalQuery, ResponseClass};
    use crate::ports::exchange::{ConnectorId, SelectionPolicy, TransportFailureClass};

    use super::{
        AttemptClass, FallbackDecision, UpstreamAttempt, aggregate, assess,
        deduplicate_connector_order,
    };

    fn query() -> CanonicalQuery {
        let mut message = Message::new(1, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        CanonicalQuery::from_message(message).unwrap()
    }

    fn connector(name: &str) -> ConnectorId {
        ConnectorId::new(name.to_owned()).unwrap()
    }

    fn response(code: ResponseCode) -> crate::dns::CanonicalResponse {
        crate::dns::CanonicalResponse::empty_response(&query(), code).unwrap()
    }

    fn failure(name: &str, retryable: bool) -> crate::ports::exchange::TransportFailure {
        crate::ports::exchange::TransportFailure {
            connector: connector(name),
            class: TransportFailureClass::Timeout,
            retryable,
            safe_context: Some("test"),
        }
    }

    fn attempt(
        index: usize,
        name: &str,
        outcome: crate::ports::exchange::UpstreamOutcome,
    ) -> UpstreamAttempt {
        UpstreamAttempt {
            attempt_index: index,
            connector: connector(name),
            outcome,
        }
    }

    #[test]
    fn classifies_response_and_retryable_failure_for_fallback() {
        let servfail =
            crate::ports::exchange::UpstreamOutcome::Response(response(ResponseCode::ServFail));
        assert_eq!(
            assess(&servfail).class,
            AttemptClass::Response(ResponseClass::ServFail)
        );
        assert_eq!(assess(&servfail).fallback, FallbackDecision::Continue);

        let failure =
            crate::ports::exchange::UpstreamOutcome::TransportFailure(failure("one", true));
        assert_eq!(
            assess(&failure).class,
            AttemptClass::TransportFailure {
                class: TransportFailureClass::Timeout,
                retryable: true,
            }
        );
        assert_eq!(assess(&failure).fallback, FallbackDecision::Continue);
    }

    #[test]
    fn all_failures_become_servfail() {
        let outcome = aggregate(
            SelectionPolicy::Failover,
            &query(),
            vec![
                attempt(
                    1,
                    "two",
                    crate::ports::exchange::UpstreamOutcome::TransportFailure(failure("two", true)),
                ),
                attempt(
                    0,
                    "one",
                    crate::ports::exchange::UpstreamOutcome::Response(response(
                        ResponseCode::ServFail,
                    )),
                ),
            ],
        )
        .unwrap();
        let crate::ports::exchange::UpstreamOutcome::Response(response) = outcome else {
            panic!("expected SERVFAIL response");
        };
        assert_eq!(response.class(), ResponseClass::ServFail);
    }

    #[test]
    fn valid_response_wins_in_fallback_order() {
        let outcome = aggregate(
            SelectionPolicy::Failover,
            &query(),
            vec![
                attempt(
                    2,
                    "three",
                    crate::ports::exchange::UpstreamOutcome::Response(response(
                        ResponseCode::NoError,
                    )),
                ),
                attempt(
                    0,
                    "one",
                    crate::ports::exchange::UpstreamOutcome::Response(response(
                        ResponseCode::ServFail,
                    )),
                ),
                attempt(
                    1,
                    "two",
                    crate::ports::exchange::UpstreamOutcome::Response(response(
                        ResponseCode::NXDomain,
                    )),
                ),
            ],
        )
        .unwrap();
        let crate::ports::exchange::UpstreamOutcome::Response(response) = outcome else {
            panic!("expected DNS response");
        };
        assert_eq!(response.class(), ResponseClass::NxDomain);
    }

    #[test]
    fn cancellation_has_priority_over_exhausted_failures() {
        let outcome = aggregate(
            SelectionPolicy::Parallel,
            &query(),
            vec![
                attempt(
                    0,
                    "one",
                    crate::ports::exchange::UpstreamOutcome::TransportFailure(failure("one", true)),
                ),
                attempt(
                    1,
                    "two",
                    crate::ports::exchange::UpstreamOutcome::Cancelled(
                        CancelReason::UpstreamCancelled,
                    ),
                ),
                attempt(
                    2,
                    "three",
                    crate::ports::exchange::UpstreamOutcome::Cancelled(CancelReason::Shutdown),
                ),
            ],
        )
        .unwrap();
        assert!(matches!(
            outcome,
            crate::ports::exchange::UpstreamOutcome::Cancelled(CancelReason::Shutdown)
        ));
    }

    #[test]
    fn external_cancellation_beats_a_collected_response() {
        let outcome = aggregate(
            SelectionPolicy::Parallel,
            &query(),
            vec![
                attempt(
                    0,
                    "one",
                    crate::ports::exchange::UpstreamOutcome::Response(response(
                        ResponseCode::NoError,
                    )),
                ),
                attempt(
                    1,
                    "two",
                    crate::ports::exchange::UpstreamOutcome::Cancelled(
                        CancelReason::DeadlineExceeded,
                    ),
                ),
            ],
        )
        .unwrap();
        assert!(matches!(
            outcome,
            crate::ports::exchange::UpstreamOutcome::Cancelled(CancelReason::DeadlineExceeded)
        ));
    }

    #[test]
    fn deduplicates_primary_and_fallback_without_changing_order() {
        let connectors = deduplicate_connector_order(
            vec![connector("one"), connector("two")],
            vec![connector("two"), connector("three"), connector("one")],
        );
        assert_eq!(
            connectors
                .iter()
                .map(ConnectorId::as_str)
                .collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
    }

    #[test]
    fn duplicate_attempt_indices_are_rejected() {
        let error = aggregate(
            SelectionPolicy::Failover,
            &query(),
            vec![
                attempt(
                    0,
                    "one",
                    crate::ports::exchange::UpstreamOutcome::Response(response(
                        ResponseCode::NoError,
                    )),
                ),
                attempt(
                    0,
                    "two",
                    crate::ports::exchange::UpstreamOutcome::Response(response(
                        ResponseCode::NoError,
                    )),
                ),
            ],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "upstream attempt index 0 is duplicated");
    }
}
