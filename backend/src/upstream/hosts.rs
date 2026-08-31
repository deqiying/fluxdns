//! 内联 hosts upstream connector。

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use hickory_proto::op::ResponseCode;
use hickory_proto::rr::{RData, Record, RecordType, rdata::A, rdata::AAAA};
use serde::Deserialize;
use thiserror::Error;

use crate::config::resolve::ResolvedUpstream;
use crate::dns::{
    CancelReason, CanonicalQuery, CanonicalResponse, DEFAULT_LOCAL_TTL, HostsTable, RequestContext,
};
use crate::ports::PortFuture;
use crate::ports::exchange::{
    ConnectorId, DnsExchange, TransportFailure, TransportFailureClass, UpstreamOutcome,
};

#[derive(Clone, Debug)]
pub struct HostsExchange {
    connector: ConnectorId,
    table: Arc<HostsTable>,
    ttl: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum HostsExchangeBuildError {
    #[error("upstream `{upstream}` is not a hosts connector")]
    NotHosts { upstream: String },
    #[error("upstream `{upstream}` has unsupported hosts format `{format}`")]
    UnsupportedFormat { upstream: String, format: String },
    #[error("upstream `{upstream}` contains invalid hosts data")]
    InvalidData { upstream: String },
    #[error("upstream `{upstream}` contains unsupported record type `{record_type}`")]
    UnsupportedRecordType {
        upstream: String,
        record_type: String,
    },
    #[error("upstream `{upstream}` contains an empty address list")]
    EmptyAddressList { upstream: String },
}

impl HostsExchange {
    pub fn from_resolved(upstream: &ResolvedUpstream) -> Result<Self, HostsExchangeBuildError> {
        let ResolvedUpstream::Hosts { id, format, hosts } = upstream else {
            return Err(HostsExchangeBuildError::NotHosts {
                upstream: upstream_id(upstream),
            });
        };

        let table = match format.as_str() {
            "hosts" => {
                HostsTable::parse(hosts).map_err(|_| HostsExchangeBuildError::InvalidData {
                    upstream: id.as_str().to_owned(),
                })?
            }
            "json" => parse_json_hosts(hosts, id.as_str())?,
            _ => {
                return Err(HostsExchangeBuildError::UnsupportedFormat {
                    upstream: id.as_str().to_owned(),
                    format: format.clone(),
                });
            }
        };

        let connector = ConnectorId::new(id.as_str().to_owned()).map_err(|_| {
            HostsExchangeBuildError::InvalidData {
                upstream: id.as_str().to_owned(),
            }
        })?;
        Ok(Self {
            connector,
            table: Arc::new(table),
            ttl: DEFAULT_LOCAL_TTL,
        })
    }

    pub fn connector_id(&self) -> &ConnectorId {
        &self.connector
    }

    pub fn table(&self) -> &HostsTable {
        &self.table
    }

    fn response(
        &self,
        query: &CanonicalQuery,
    ) -> Result<CanonicalResponse, crate::dns::CanonicalMessageError> {
        let question = query.question();
        let (code, records) = match self.table.lookup(question.name(), question.query_type()) {
            Some(addresses) => (
                ResponseCode::NoError,
                addresses
                    .into_iter()
                    .filter_map(|address| match (question.query_type(), address) {
                        (RecordType::A, IpAddr::V4(address)) => Some(Record::from_rdata(
                            question.name().clone(),
                            self.ttl,
                            RData::A(A(address)),
                        )),
                        (RecordType::AAAA, IpAddr::V6(address)) => Some(Record::from_rdata(
                            question.name().clone(),
                            self.ttl,
                            RData::AAAA(AAAA(address)),
                        )),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
            ),
            None if self.table.contains_name(question.name()) => {
                (ResponseCode::NoError, Vec::new())
            }
            None => (ResponseCode::NXDomain, Vec::new()),
        };

        if records.is_empty() {
            CanonicalResponse::response_with_code(query, code, std::iter::empty())
        } else {
            CanonicalResponse::response_with_answers(query, records)
        }
    }
}

impl DnsExchange for HostsExchange {
    fn connector_id(&self) -> &ConnectorId {
        &self.connector
    }

    fn exchange<'a>(
        &'a self,
        query: &'a CanonicalQuery,
        context: &'a RequestContext,
    ) -> PortFuture<'a, UpstreamOutcome> {
        Box::pin(async move {
            if context.meta.cancellation.is_cancelled() {
                return UpstreamOutcome::Cancelled(
                    context
                        .meta
                        .cancellation
                        .reason()
                        .unwrap_or(CancelReason::UpstreamCancelled),
                );
            }
            if context.meta.deadline.is_expired(Instant::now()) {
                return UpstreamOutcome::TransportFailure(TransportFailure {
                    connector: self.connector.clone(),
                    class: TransportFailureClass::Timeout,
                    retryable: true,
                    safe_context: Some("request deadline expired"),
                });
            }

            match self.response(query) {
                Ok(response) => UpstreamOutcome::Response(response),
                Err(_) => UpstreamOutcome::TransportFailure(TransportFailure {
                    connector: self.connector.clone(),
                    class: TransportFailureClass::Internal,
                    retryable: false,
                    safe_context: Some("hosts response construction failed"),
                }),
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JsonAddresses {
    Single(String),
    Multiple(Vec<String>),
}

type JsonHosts = BTreeMap<String, BTreeMap<String, JsonAddresses>>;

fn parse_json_hosts(input: &str, upstream: &str) -> Result<HostsTable, HostsExchangeBuildError> {
    let parsed: JsonHosts =
        yaml_serde::from_str(input).map_err(|_| HostsExchangeBuildError::InvalidData {
            upstream: upstream.to_owned(),
        })?;
    let mut hosts = String::new();
    for (name, records) in parsed {
        if records.is_empty() {
            return Err(HostsExchangeBuildError::EmptyAddressList {
                upstream: upstream.to_owned(),
            });
        }
        for (record_type, addresses) in records {
            let expects_ipv6 = match record_type.as_str() {
                "A" => false,
                "AAAA" => true,
                _ => {
                    return Err(HostsExchangeBuildError::UnsupportedRecordType {
                        upstream: upstream.to_owned(),
                        record_type,
                    });
                }
            };
            let values = match addresses {
                JsonAddresses::Single(value) => vec![value],
                JsonAddresses::Multiple(values) => values,
            };
            if values.is_empty() {
                return Err(HostsExchangeBuildError::EmptyAddressList {
                    upstream: upstream.to_owned(),
                });
            }
            for value in values {
                let address =
                    value
                        .parse::<IpAddr>()
                        .map_err(|_| HostsExchangeBuildError::InvalidData {
                            upstream: upstream.to_owned(),
                        })?;
                if address.is_ipv6() != expects_ipv6 {
                    return Err(HostsExchangeBuildError::InvalidData {
                        upstream: upstream.to_owned(),
                    });
                }
                hosts.push_str(&address.to_string());
                hosts.push(' ');
                hosts.push_str(&name);
                hosts.push('\n');
            }
        }
    }
    HostsTable::parse(&hosts).map_err(|_| HostsExchangeBuildError::InvalidData {
        upstream: upstream.to_owned(),
    })
}

fn upstream_id(upstream: &ResolvedUpstream) -> String {
    match upstream {
        ResolvedUpstream::Hosts { id, .. }
        | ResolvedUpstream::Doh { id, .. }
        | ResolvedUpstream::Group { id, .. } => id.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::str::FromStr;
    use std::time::{Duration, SystemTime};

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    use crate::config::resolve::{ConfigId, ResolvedUpstream};
    use crate::dns::{
        CacheCompatibilityKey, CancelReason, Cancellation, CanonicalQuery, ClientIdentity,
        Deadline, DnsMessageId, ListenerId, RequestContext, RequestId, RequestMeta,
        RuntimeRevision, TransportCapabilities, TransportClass,
    };
    use crate::ports::exchange::{DnsExchange, TransportFailureClass, UpstreamOutcome};

    use super::{HostsExchange, HostsExchangeBuildError};

    fn upstream(format: &str, hosts: &str) -> ResolvedUpstream {
        ResolvedUpstream::Hosts {
            id: ConfigId::new("local".to_owned()).unwrap(),
            format: format.to_owned(),
            hosts: hosts.to_owned(),
        }
    }

    fn query(name: &str, record_type: RecordType) -> CanonicalQuery {
        let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(Name::from_str(name).unwrap(), record_type));
        CanonicalQuery::from_message(message).unwrap()
    }

    fn context(deadline: Deadline, cancellation: Cancellation) -> RequestContext {
        let now = std::time::Instant::now();
        RequestContext {
            meta: RequestMeta {
                request_id: RequestId(1),
                trace_id: None,
                received_at: now,
                received_at_utc: SystemTime::now(),
                deadline,
                cancellation,
                connection_id: None,
                stream_id: None,
                listener_id: ListenerId::from("test"),
                route_id: None,
                original_dns_id: Some(0x1234),
            },
            client: ClientIdentity {
                peer_addr: Some(SocketAddr::from(([127, 0, 0, 1], 5300))),
                ..ClientIdentity::default()
            },
            transport: TransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            },
            runtime_revision: RuntimeRevision(1),
        }
    }

    #[tokio::test]
    async fn parses_hosts_and_json_formats() {
        let text =
            HostsExchange::from_resolved(&upstream("hosts", "192.0.2.1 example.test\n")).unwrap();
        let json = HostsExchange::from_resolved(&upstream(
            "json",
            r#"{"example.test":{"A":"192.0.2.1","AAAA":["2001:db8::1"]}}"#,
        ))
        .unwrap();
        assert_eq!(text.table().name_count(), 1);
        assert_eq!(json.table().address_count(), 2);
    }

    #[test]
    fn rejects_non_address_json_records_explicitly() {
        let error = HostsExchange::from_resolved(&upstream(
            "json",
            r#"{"example.test":{"CNAME":"alias.test"}}"#,
        ))
        .unwrap_err();
        assert_eq!(
            error,
            HostsExchangeBuildError::UnsupportedRecordType {
                upstream: "local".to_owned(),
                record_type: "CNAME".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn returns_positive_nodata_nxdomain_and_canonical_zero_id() {
        let exchange = HostsExchange::from_resolved(&upstream(
            "hosts",
            "192.0.2.1 example.test\n2001:db8::1 example.test\n",
        ))
        .unwrap();
        let deadline = Deadline::new(std::time::Instant::now() + Duration::from_secs(30));
        let positive = exchange
            .exchange(
                &query("example.test", RecordType::A),
                &context(deadline, Cancellation::new()),
            )
            .await;
        let nodata = exchange
            .exchange(
                &query("example.test", RecordType::MX),
                &context(deadline, Cancellation::new()),
            )
            .await;
        let nxdomain = exchange
            .exchange(
                &query("missing.test", RecordType::A),
                &context(deadline, Cancellation::new()),
            )
            .await;

        let UpstreamOutcome::Response(positive) = positive else {
            panic!("expected positive response");
        };
        assert_eq!(positive.class(), crate::dns::ResponseClass::Positive);
        assert_eq!(
            positive.as_message().metadata.id,
            DnsMessageId::new(0).value()
        );
        assert!(matches!(
            nodata,
            UpstreamOutcome::Response(response)
                if response.class() == crate::dns::ResponseClass::NoData
        ));
        assert!(matches!(
            nxdomain,
            UpstreamOutcome::Response(response)
                if response.class() == crate::dns::ResponseClass::NxDomain
        ));
    }

    #[tokio::test]
    async fn returns_cancelled_and_timeout_outcomes() {
        let exchange =
            HostsExchange::from_resolved(&upstream("hosts", "192.0.2.1 example.test\n")).unwrap();
        let now = std::time::Instant::now();
        let cancellation = Cancellation::new();
        cancellation.cancel(CancelReason::ClientDisconnected);
        assert!(matches!(
            exchange
                .exchange(
                    &query("example.test", RecordType::A),
                    &context(Deadline::new(now + Duration::from_secs(30)), cancellation),
                )
                .await,
            UpstreamOutcome::Cancelled(CancelReason::ClientDisconnected)
        ));
        assert!(matches!(
            exchange
                .exchange(
                    &query("example.test", RecordType::A),
                    &context(Deadline::new(now - Duration::from_secs(1)), Cancellation::new()),
                )
                .await,
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::Timeout
        ));
    }
}
