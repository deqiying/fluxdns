//! UDP 入站 adapter。

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use thiserror::Error;

use crate::config::{BindProtocol, BindTransport};
use crate::dns::{
    Cancellation, ClientIdentity, Deadline, DnsMessageId, DnsRequest, ListenerId, RequestContext,
    RequestId, RequestMeta, RuntimeRevision, TransportCapabilities, TransportClass,
};
use crate::ports::effects::{ActivatedSocketHandle, UdpSocketHandle};
use crate::ports::inbound::{InboundAdapter, InboundRequest, ResponseEncoder};
use crate::ports::{PortError, PortErrorClass, PortFuture};

use super::wire::{MAX_DNS_WIRE_BYTES, WireError, decode_query, encode_response_truncated};
use crate::runtime::BoundEndpointHandle;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum UdpAdapterError {
    #[error("UDP adapter requires a positive request timeout")]
    InvalidTimeout,
    #[error("bind endpoint and activated socket protocol do not match")]
    ProtocolMismatch,
    #[error("UDP adapter requires datagram transport capabilities")]
    InvalidTransportClass,
}

/// 绑定到一个已激活 UDP capability 的入站 adapter。
#[derive(Clone)]
pub struct UdpAdapter {
    socket: Arc<dyn UdpSocketHandle>,
    listener_id: ListenerId,
    runtime_revision: RuntimeRevision,
    transport: TransportCapabilities,
    request_ids: Arc<AtomicU64>,
    request_timeout: Duration,
}

impl fmt::Debug for UdpAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UdpAdapter")
            .field("listener_id", &self.listener_id)
            .field("runtime_revision", &self.runtime_revision)
            .field("transport", &self.transport)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl UdpAdapter {
    pub fn from_endpoint(
        endpoint: BoundEndpointHandle,
        runtime_revision: RuntimeRevision,
        transport: TransportCapabilities,
        request_timeout: Duration,
    ) -> Result<Self, UdpAdapterError> {
        if endpoint.entry.protocol != BindProtocol::Udp
            || endpoint.entry.transport != BindTransport::Udp
        {
            return Err(UdpAdapterError::ProtocolMismatch);
        }
        let ActivatedSocketHandle::Udp(socket) = endpoint.socket else {
            return Err(UdpAdapterError::ProtocolMismatch);
        };
        Self::new(
            socket,
            ListenerId::from(endpoint.entry.owner),
            runtime_revision,
            transport,
            request_timeout,
        )
    }

    pub fn new(
        socket: Arc<dyn UdpSocketHandle>,
        listener_id: ListenerId,
        runtime_revision: RuntimeRevision,
        transport: TransportCapabilities,
        request_timeout: Duration,
    ) -> Result<Self, UdpAdapterError> {
        if request_timeout.is_zero() {
            return Err(UdpAdapterError::InvalidTimeout);
        }
        if transport.class != TransportClass::Datagram {
            return Err(UdpAdapterError::InvalidTransportClass);
        }
        Ok(Self {
            socket,
            listener_id,
            runtime_revision,
            transport,
            request_ids: Arc::new(AtomicU64::new(0)),
            request_timeout,
        })
    }

    pub fn listener_id(&self) -> &ListenerId {
        &self.listener_id
    }
}

impl InboundAdapter for UdpAdapter {
    fn receive<'a>(
        &'a self,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Option<InboundRequest>, PortError>> {
        Box::pin(async move {
            loop {
                if cancellation.is_cancelled() {
                    return Ok(None);
                }

                let received_at = Instant::now();
                let deadline = Deadline::new(received_at + self.request_timeout);
                let datagram = match self
                    .socket
                    .recv_from(MAX_DNS_WIRE_BYTES, deadline, cancellation)
                    .await
                {
                    Ok(datagram) => datagram,
                    Err(error) if matches!(error.class(), PortErrorClass::Cancelled(_)) => {
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                };

                let parsed = match decode_query(&datagram.payload, MAX_DNS_WIRE_BYTES) {
                    Ok(parsed) => parsed,
                    Err(_) => continue,
                };

                let request_id = RequestId::from(
                    self.request_ids
                        .fetch_add(1, Ordering::AcqRel)
                        .wrapping_add(1) as u128,
                );
                let original_dns_id = parsed.id.value();
                let context = RequestContext {
                    meta: RequestMeta {
                        request_id,
                        trace_id: None,
                        received_at,
                        received_at_utc: SystemTime::now(),
                        deadline,
                        cancellation: Cancellation::new(),
                        connection_id: None,
                        stream_id: None,
                        listener_id: self.listener_id.clone(),
                        route_id: None,
                        original_dns_id: Some(original_dns_id),
                    },
                    client: ClientIdentity {
                        peer_addr: Some(datagram.peer),
                        client_addr: Some(datagram.peer.ip()),
                        client_id: None,
                    },
                    transport: self.transport,
                    runtime_revision: self.runtime_revision,
                };
                let encoder = Arc::new(UdpResponseEncoder {
                    socket: Arc::clone(&self.socket),
                    peer: datagram.peer,
                });
                return Ok(Some(InboundRequest::new(
                    DnsRequest {
                        query: parsed.query,
                        context,
                    },
                    encoder,
                )));
            }
        })
    }
}

struct UdpResponseEncoder {
    socket: Arc<dyn UdpSocketHandle>,
    peer: SocketAddr,
}

impl ResponseEncoder for UdpResponseEncoder {
    fn encode<'a>(
        &'a self,
        request: &'a DnsRequest,
        response: crate::dns::CanonicalResponse,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            let id = request
                .context
                .meta
                .original_dns_id
                .map(DnsMessageId::new)
                .ok_or_else(|| {
                    PortError::new(PortErrorClass::ProtocolViolation, "transport.udp.encode")
                })?;
            let max_bytes = request.query.as_message().max_payload().into();
            let bytes =
                encode_response_truncated(&response, id, max_bytes).map_err(map_wire_error)?;
            self.socket
                .send_to(
                    bytes,
                    self.peer,
                    request.context.meta.deadline,
                    &request.context.meta.cancellation,
                )
                .await
        })
    }
}

fn map_wire_error(error: WireError) -> PortError {
    let class = match error {
        WireError::TooLarge { .. } => PortErrorClass::ResourceExhausted,
        WireError::Empty | WireError::Decode | WireError::InvalidQuery(_) => {
            PortErrorClass::ProtocolViolation
        }
        WireError::Encode => PortErrorClass::Internal,
    };
    PortError::new(class, "transport.udp.encode")
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RData, Record, RecordType, rdata::A};

    use crate::dns::{
        CacheCompatibilityKey, Cancellation, CanonicalResponse, RuntimeRevision, ServFailCore,
        TransportCapabilities, TransportClass, dispatch_inbound,
    };
    use crate::ports::effects::{UdpDatagram, UdpSocketHandle};
    use crate::ports::inbound::InboundAdapter;
    use crate::ports::{PortError, PortErrorClass, PortFuture};

    use super::{DEFAULT_REQUEST_TIMEOUT, UdpAdapter};

    #[derive(Default)]
    struct FakeUdpSocket {
        incoming: Mutex<VecDeque<Result<UdpDatagram, PortError>>>,
        sent: Mutex<Vec<(Vec<u8>, SocketAddr)>>,
    }

    impl FakeUdpSocket {
        fn push(&self, datagram: Result<UdpDatagram, PortError>) {
            self.incoming.lock().unwrap().push_back(datagram);
        }

        fn sent(&self) -> Vec<(Vec<u8>, SocketAddr)> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl UdpSocketHandle for FakeUdpSocket {
        fn local_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(SocketAddr::from(([127, 0, 0, 1], 8353)))
        }

        fn recv_from<'a>(
            &'a self,
            _max_bytes: usize,
            _deadline: crate::dns::Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<UdpDatagram, PortError>> {
            let result = self
                .incoming
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Err(PortError::new(
                        PortErrorClass::Cancelled(crate::dns::CancelReason::Shutdown),
                        "test.udp.recv",
                    ))
                });
            Box::pin(async move { result })
        }

        fn send_to<'a>(
            &'a self,
            payload: Vec<u8>,
            target: SocketAddr,
            _deadline: crate::dns::Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<(), PortError>> {
            self.sent.lock().unwrap().push((payload, target));
            Box::pin(async { Ok(()) })
        }
    }

    fn wire_query(id: u16) -> Vec<u8> {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_ascii("example.com.").unwrap(),
            RecordType::A,
        ));
        message.to_vec().unwrap()
    }

    fn adapter(socket: Arc<FakeUdpSocket>) -> UdpAdapter {
        let socket: Arc<dyn UdpSocketHandle> = socket;
        UdpAdapter::new(
            socket,
            "dns-udp".into(),
            RuntimeRevision(3),
            TransportCapabilities {
                class: TransportClass::Datagram,
                cache_compatibility: CacheCompatibilityKey(1),
            },
            DEFAULT_REQUEST_TIMEOUT,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn drops_malformed_datagrams_and_builds_canonical_request() {
        let socket = Arc::new(FakeUdpSocket::default());
        let peer = SocketAddr::from(([192, 0, 2, 10], 53000));
        socket.push(Ok(UdpDatagram {
            payload: vec![0xff, 0x00],
            peer,
        }));
        socket.push(Ok(UdpDatagram {
            payload: wire_query(0xbeef),
            peer,
        }));
        let adapter = adapter(Arc::clone(&socket));

        let inbound = adapter
            .receive(&Cancellation::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbound.request().query.as_message().metadata.id, 0);
        assert_eq!(inbound.request().context.meta.original_dns_id, Some(0xbeef));
        assert_eq!(inbound.request().context.client.peer_addr, Some(peer));
        assert_eq!(
            inbound.request().context.client.client_addr,
            Some(peer.ip())
        );
        assert_eq!(
            inbound.request().context.runtime_revision,
            RuntimeRevision(3)
        );
    }

    #[tokio::test]
    async fn dispatches_servfail_and_restores_udp_dns_id() {
        let socket = Arc::new(FakeUdpSocket::default());
        let peer = SocketAddr::from(([192, 0, 2, 11], 53001));
        socket.push(Ok(UdpDatagram {
            payload: wire_query(0x1234),
            peer,
        }));
        let adapter = adapter(Arc::clone(&socket));
        let inbound = adapter
            .receive(&Cancellation::new())
            .await
            .unwrap()
            .unwrap();

        dispatch_inbound(&ServFailCore, inbound).await.unwrap();

        let sent = socket.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, peer);
        let response = Message::from_vec(&sent[0].0).unwrap();
        assert_eq!(response.metadata.id, 0x1234);
        assert_eq!(response.metadata.response_code, ResponseCode::ServFail);
    }

    #[tokio::test]
    async fn truncates_large_response_at_client_udp_limit() {
        let socket = Arc::new(FakeUdpSocket::default());
        let peer = SocketAddr::from(([192, 0, 2, 12], 53002));
        socket.push(Ok(UdpDatagram {
            payload: wire_query(0x4321),
            peer,
        }));
        let adapter = adapter(Arc::clone(&socket));
        let inbound = adapter
            .receive(&Cancellation::new())
            .await
            .unwrap()
            .unwrap();
        let query = inbound.request().query.clone();
        let answers = (1..=40_u8).map(|octet| {
            Record::from_rdata(
                query.question().name().clone(),
                60,
                RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, octet))),
            )
        });
        let response = CanonicalResponse::response_with_answers(&query, answers).unwrap();

        inbound.response().respond(response).await.unwrap();

        let sent = socket.sent();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].0.len() <= 512);
        let response = Message::from_vec(&sent[0].0).unwrap();
        assert_eq!(response.metadata.id, 0x4321);
        assert!(response.metadata.truncation);
        assert!(response.answers.len() < 40);
    }

    #[tokio::test]
    async fn cancellation_stops_receive_without_returning_socket_error() {
        let socket = Arc::new(FakeUdpSocket::default());
        let adapter = adapter(socket);
        let cancellation = Cancellation::new();
        cancellation.cancel(crate::dns::CancelReason::Shutdown);

        assert!(adapter.receive(&cancellation).await.unwrap().is_none());
    }

    #[test]
    fn rejects_zero_timeout_and_non_datagram_capability() {
        let socket = Arc::new(FakeUdpSocket::default());
        let socket_handle: Arc<dyn UdpSocketHandle> = socket.clone();
        assert_eq!(
            UdpAdapter::new(
                socket_handle,
                "dns-udp".into(),
                RuntimeRevision(1),
                TransportCapabilities {
                    class: TransportClass::Datagram,
                    cache_compatibility: CacheCompatibilityKey(1),
                },
                Duration::ZERO,
            )
            .unwrap_err(),
            super::UdpAdapterError::InvalidTimeout
        );
        let socket_handle: Arc<dyn UdpSocketHandle> = socket;
        assert_eq!(
            UdpAdapter::new(
                socket_handle,
                "dns-udp".into(),
                RuntimeRevision(1),
                TransportCapabilities {
                    class: TransportClass::Stream,
                    cache_compatibility: CacheCompatibilityKey(1),
                },
                DEFAULT_REQUEST_TIMEOUT,
            )
            .unwrap_err(),
            super::UdpAdapterError::InvalidTransportClass
        );
    }
}
