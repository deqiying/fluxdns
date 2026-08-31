//! TCP DNS framing 边界。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use thiserror::Error;
use tokio::sync::Mutex;

use crate::config::BindProtocol;
use crate::dns::{
    Cancellation, ClientIdentity, ConnectionId, Deadline, DnsMessageId, DnsRequest, ListenerId,
    RequestContext, RequestId, RequestMeta, RuntimeRevision, StreamId, TransportCapabilities,
    TransportClass,
};
use crate::ports::effects::{
    ActivatedSocketHandle, TcpConnectionHandle, TcpListenerHandle, TcpReadResult,
};
use crate::ports::inbound::{InboundAdapter, InboundRequest, ResponseEncoder};
use crate::ports::{PortError, PortErrorClass, PortFuture};
use crate::runtime::BoundEndpointHandle;

use super::wire::MAX_DNS_WIRE_BYTES;

pub const TCP_FRAME_PREFIX_BYTES: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum TcpFrameError {
    #[error("TCP DNS frame length must be greater than zero")]
    Empty,
    #[error("TCP DNS frame exceeds the {limit} byte limit")]
    TooLarge { limit: usize },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum TcpAdapterError {
    #[error("TCP adapter requires a positive request timeout")]
    InvalidTimeout,
    #[error("bind endpoint and activated socket protocol do not match")]
    ProtocolMismatch,
    #[error("TCP adapter requires stream transport capabilities")]
    InvalidTransportClass,
}

/// TCP 入站 adapter；当前每个 accept 只处理一个 DNS frame，响应后关闭连接。
pub struct TcpAdapter {
    listener: Arc<dyn TcpListenerHandle>,
    listener_id: ListenerId,
    runtime_revision: RuntimeRevision,
    transport: TransportCapabilities,
    request_ids: AtomicU64,
    connection_ids: AtomicU64,
    request_timeout: Duration,
}

impl fmt::Debug for TcpAdapter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TcpAdapter")
            .field("listener_id", &self.listener_id)
            .field("runtime_revision", &self.runtime_revision)
            .field("transport", &self.transport)
            .field("request_timeout", &self.request_timeout)
            .finish_non_exhaustive()
    }
}

impl TcpAdapter {
    pub fn from_endpoint(
        endpoint: BoundEndpointHandle,
        runtime_revision: RuntimeRevision,
        transport: TransportCapabilities,
        request_timeout: Duration,
    ) -> Result<Self, TcpAdapterError> {
        if endpoint.entry.protocol != BindProtocol::Tcp {
            return Err(TcpAdapterError::ProtocolMismatch);
        }
        let ActivatedSocketHandle::Tcp(listener) = endpoint.socket else {
            return Err(TcpAdapterError::ProtocolMismatch);
        };
        Self::new(
            listener,
            ListenerId::from(endpoint.entry.owner),
            runtime_revision,
            transport,
            request_timeout,
        )
    }

    pub fn new(
        listener: Arc<dyn TcpListenerHandle>,
        listener_id: ListenerId,
        runtime_revision: RuntimeRevision,
        transport: TransportCapabilities,
        request_timeout: Duration,
    ) -> Result<Self, TcpAdapterError> {
        if request_timeout.is_zero() {
            return Err(TcpAdapterError::InvalidTimeout);
        }
        if transport.class != TransportClass::Stream {
            return Err(TcpAdapterError::InvalidTransportClass);
        }
        Ok(Self {
            listener,
            listener_id,
            runtime_revision,
            transport,
            request_ids: AtomicU64::new(0),
            connection_ids: AtomicU64::new(0),
            request_timeout,
        })
    }
}

impl InboundAdapter for TcpAdapter {
    fn receive<'a>(
        &'a self,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Option<InboundRequest>, PortError>> {
        Box::pin(async move {
            loop {
                if cancellation.is_cancelled() {
                    return Ok(None);
                }

                let accept_deadline = Deadline::new(Instant::now() + self.request_timeout);
                let Some(mut connection) =
                    self.listener.accept(accept_deadline, cancellation).await?
                else {
                    return Ok(None);
                };
                let peer = match connection.peer_addr() {
                    Ok(peer) => peer,
                    Err(_) => {
                        close_connection(&mut connection).await;
                        continue;
                    }
                };
                let connection_id = ConnectionId::from(
                    self.connection_ids
                        .fetch_add(1, Ordering::AcqRel)
                        .wrapping_add(1),
                );

                let received_at = Instant::now();
                let deadline = Deadline::new(received_at + self.request_timeout);
                let prefix = match connection
                    .read_exact(TCP_FRAME_PREFIX_BYTES, deadline, cancellation)
                    .await
                {
                    Ok(TcpReadResult::Complete(prefix)) => prefix,
                    Ok(TcpReadResult::CleanEof) => {
                        close_connection(&mut connection).await;
                        continue;
                    }
                    Err(_) if cancellation.is_cancelled() => return Ok(None),
                    Err(_) => {
                        close_connection(&mut connection).await;
                        continue;
                    }
                };
                let prefix: [u8; TCP_FRAME_PREFIX_BYTES] = prefix.try_into().map_err(|_| {
                    PortError::new(
                        PortErrorClass::ProtocolViolation,
                        "transport.tcp.read_prefix",
                    )
                })?;
                let frame_length = match decode_frame_length(prefix) {
                    Ok(length) => length,
                    Err(_) => {
                        close_connection(&mut connection).await;
                        continue;
                    }
                };
                let payload = match connection
                    .read_exact(frame_length, deadline, cancellation)
                    .await
                {
                    Ok(TcpReadResult::Complete(payload)) => payload,
                    Ok(TcpReadResult::CleanEof) => {
                        close_connection(&mut connection).await;
                        continue;
                    }
                    Err(_) if cancellation.is_cancelled() => return Ok(None),
                    Err(_) => {
                        close_connection(&mut connection).await;
                        continue;
                    }
                };
                let parsed = match super::wire::decode_query(&payload, MAX_DNS_WIRE_BYTES) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        close_connection(&mut connection).await;
                        continue;
                    }
                };

                let request_id = RequestId::from(
                    self.request_ids
                        .fetch_add(1, Ordering::AcqRel)
                        .wrapping_add(1) as u128,
                );
                let context = RequestContext {
                    meta: RequestMeta {
                        request_id,
                        trace_id: None,
                        received_at,
                        received_at_utc: SystemTime::now(),
                        deadline,
                        cancellation: Cancellation::new(),
                        connection_id: Some(connection_id),
                        stream_id: Some(StreamId::from(1)),
                        listener_id: self.listener_id.clone(),
                        route_id: None,
                        original_dns_id: Some(parsed.id.value()),
                    },
                    client: ClientIdentity {
                        peer_addr: Some(peer),
                        client_addr: Some(peer.ip()),
                        client_id: None,
                    },
                    transport: self.transport,
                    runtime_revision: self.runtime_revision,
                };
                let connection = Arc::new(Mutex::new(connection));
                let encoder = Arc::new(TcpResponseEncoder { connection });
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

async fn close_connection(connection: &mut Box<dyn TcpConnectionHandle>) {
    let _ = connection.shutdown().await;
}

struct TcpResponseEncoder {
    connection: Arc<Mutex<Box<dyn TcpConnectionHandle>>>,
}

impl ResponseEncoder for TcpResponseEncoder {
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
                    PortError::new(PortErrorClass::ProtocolViolation, "transport.tcp.encode")
                })?;
            let payload = super::wire::encode_response(&response, id, MAX_DNS_WIRE_BYTES)
                .map_err(map_wire_error)?;
            let frame = encode_frame(&payload, MAX_DNS_WIRE_BYTES).map_err(map_frame_error)?;
            let mut connection = self.connection.lock().await;
            connection
                .write_all(
                    frame,
                    request.context.meta.deadline,
                    &request.context.meta.cancellation,
                )
                .await
        })
    }
}

fn map_frame_error(error: TcpFrameError) -> PortError {
    let class = match error {
        TcpFrameError::Empty => PortErrorClass::ProtocolViolation,
        TcpFrameError::TooLarge { .. } => PortErrorClass::ResourceExhausted,
    };
    PortError::new(class, "transport.tcp.frame")
}

fn map_wire_error(error: super::wire::WireError) -> PortError {
    let class = match error {
        super::wire::WireError::TooLarge { .. } => PortErrorClass::ResourceExhausted,
        super::wire::WireError::Empty
        | super::wire::WireError::Decode
        | super::wire::WireError::InvalidQuery(_) => PortErrorClass::ProtocolViolation,
        super::wire::WireError::Encode => PortErrorClass::Internal,
    };
    PortError::new(class, "transport.tcp.encode")
}

/// 解码网络序 length prefix；零长度 frame 不表示 EOF，而是协议错误。
pub fn decode_frame_length(prefix: [u8; TCP_FRAME_PREFIX_BYTES]) -> Result<usize, TcpFrameError> {
    let length = u16::from_be_bytes(prefix) as usize;
    if length == 0 {
        return Err(TcpFrameError::Empty);
    }
    Ok(length)
}

/// 为 DNS wire payload 添加两字节网络序长度前缀。
pub fn encode_frame(payload: &[u8], max_bytes: usize) -> Result<Vec<u8>, TcpFrameError> {
    let limit = max_bytes.min(MAX_DNS_WIRE_BYTES);
    if payload.is_empty() {
        return Err(TcpFrameError::Empty);
    }
    if payload.len() > limit {
        return Err(TcpFrameError::TooLarge { limit });
    }

    let length = u16::try_from(payload.len()).map_err(|_| TcpFrameError::TooLarge {
        limit: MAX_DNS_WIRE_BYTES,
    })?;
    let mut frame = Vec::with_capacity(TCP_FRAME_PREFIX_BYTES + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::dns::{
        CacheCompatibilityKey, Cancellation, RuntimeRevision, ServFailCore, TransportCapabilities,
        TransportClass, dispatch_inbound,
    };
    use crate::ports::effects::{TcpConnectionHandle, TcpListenerHandle, TcpReadResult};
    use crate::ports::inbound::InboundAdapter;
    use crate::ports::{PortError, PortErrorClass, PortFuture};

    use super::{MAX_DNS_WIRE_BYTES, TcpAdapter, TcpFrameError, decode_frame_length, encode_frame};

    struct FakeTcpConnection {
        read_buffer: Mutex<Vec<u8>>,
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        peer: SocketAddr,
    }

    impl FakeTcpConnection {
        fn new(frame: Vec<u8>, writes: Arc<Mutex<Vec<Vec<u8>>>>, peer: SocketAddr) -> Self {
            Self {
                read_buffer: Mutex::new(frame),
                writes,
                peer,
            }
        }
    }

    impl TcpConnectionHandle for FakeTcpConnection {
        fn peer_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(self.peer)
        }

        fn read_exact<'a>(
            &'a mut self,
            length: usize,
            _deadline: crate::dns::Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<TcpReadResult, PortError>> {
            let result = {
                let mut buffer = self.read_buffer.lock().unwrap();
                if buffer.is_empty() {
                    Ok(TcpReadResult::CleanEof)
                } else if buffer.len() < length {
                    Err(PortError::new(
                        PortErrorClass::ProtocolViolation,
                        "test.tcp.read_exact",
                    ))
                } else {
                    Ok(TcpReadResult::Complete(buffer.drain(..length).collect()))
                }
            };
            Box::pin(async move { result })
        }

        fn write_all<'a>(
            &'a mut self,
            payload: Vec<u8>,
            _deadline: crate::dns::Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<(), PortError>> {
            self.writes.lock().unwrap().push(payload);
            Box::pin(async { Ok(()) })
        }

        fn shutdown(&mut self) -> PortFuture<'_, Result<(), PortError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct FakeTcpListener {
        connections: Mutex<VecDeque<Box<dyn TcpConnectionHandle>>>,
    }

    impl FakeTcpListener {
        fn push(&self, connection: Box<dyn TcpConnectionHandle>) {
            self.connections.lock().unwrap().push_back(connection);
        }
    }

    impl TcpListenerHandle for FakeTcpListener {
        fn local_addr(&self) -> Result<SocketAddr, PortError> {
            Ok(SocketAddr::from(([127, 0, 0, 1], 8353)))
        }

        fn accept<'a>(
            &'a self,
            _deadline: crate::dns::Deadline,
            _cancellation: &'a Cancellation,
        ) -> PortFuture<'a, Result<Option<Box<dyn TcpConnectionHandle>>, PortError>> {
            let connection = self.connections.lock().unwrap().pop_front();
            Box::pin(async move { Ok(connection) })
        }
    }

    #[test]
    fn encodes_and_decodes_network_order_length_prefix() {
        let payload = b"dns";
        let frame = encode_frame(payload, 512).unwrap();

        assert_eq!(frame[..2], [0, 3]);
        assert_eq!(decode_frame_length(frame[..2].try_into().unwrap()), Ok(3));
        assert_eq!(&frame[2..], payload);
    }

    #[test]
    fn rejects_empty_frame_and_payload() {
        assert_eq!(decode_frame_length([0, 0]), Err(TcpFrameError::Empty));
        assert_eq!(encode_frame(&[], 512), Err(TcpFrameError::Empty));
    }

    #[test]
    fn enforces_caller_limit_and_absolute_dns_limit() {
        assert_eq!(
            encode_frame(&[0_u8; 513], 512),
            Err(TcpFrameError::TooLarge { limit: 512 })
        );
        assert_eq!(
            encode_frame(&vec![0_u8; MAX_DNS_WIRE_BYTES + 1], MAX_DNS_WIRE_BYTES + 1),
            Err(TcpFrameError::TooLarge {
                limit: MAX_DNS_WIRE_BYTES
            })
        );
    }

    fn wire_query(id: u16) -> Vec<u8> {
        use hickory_proto::op::{Message, MessageType, OpCode, Query};
        use hickory_proto::rr::{Name, RecordType};

        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_ascii("example.com.").unwrap(),
            RecordType::A,
        ));
        message.to_vec().unwrap()
    }

    fn adapter(listener: Arc<FakeTcpListener>) -> TcpAdapter {
        let listener: Arc<dyn TcpListenerHandle> = listener;
        TcpAdapter::new(
            listener,
            "dns-tcp".into(),
            RuntimeRevision(4),
            TransportCapabilities {
                class: TransportClass::Stream,
                cache_compatibility: CacheCompatibilityKey(1),
            },
            Duration::from_secs(5),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn receives_one_frame_and_restores_id_on_response() {
        let listener = Arc::new(FakeTcpListener {
            connections: Mutex::new(VecDeque::new()),
        });
        let writes = Arc::new(Mutex::new(Vec::new()));
        let peer = SocketAddr::from(([192, 0, 2, 12], 53002));
        let query = wire_query(0xabcd);
        let frame = encode_frame(&query, MAX_DNS_WIRE_BYTES).unwrap();
        listener.push(Box::new(FakeTcpConnection::new(
            frame,
            Arc::clone(&writes),
            peer,
        )));
        let adapter = adapter(listener);

        let inbound = adapter
            .receive(&Cancellation::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbound.request().context.meta.original_dns_id, Some(0xabcd));
        assert_eq!(
            inbound.request().context.meta.connection_id,
            Some(crate::dns::ConnectionId(1))
        );
        dispatch_inbound(&ServFailCore, inbound).await.unwrap();

        let writes = writes.lock().unwrap();
        assert_eq!(writes.len(), 1);
        let length = decode_frame_length(writes[0][..2].try_into().unwrap()).unwrap();
        let response = hickory_proto::op::Message::from_vec(&writes[0][2..]).unwrap();
        assert_eq!(length, writes[0].len() - 2);
        assert_eq!(response.metadata.id, 0xabcd);
        assert_eq!(
            response.metadata.response_code,
            hickory_proto::op::ResponseCode::ServFail
        );
    }

    #[tokio::test]
    async fn rejects_zero_length_frame_as_protocol_error() {
        let listener = Arc::new(FakeTcpListener {
            connections: Mutex::new(VecDeque::new()),
        });
        listener.push(Box::new(FakeTcpConnection::new(
            vec![0, 0],
            Arc::new(Mutex::new(Vec::new())),
            SocketAddr::from(([192, 0, 2, 13], 53003)),
        )));
        let result = adapter(listener)
            .receive(&Cancellation::new())
            .await
            .unwrap();

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn skips_clean_eof_and_accepts_the_next_connection() {
        let listener = Arc::new(FakeTcpListener {
            connections: Mutex::new(VecDeque::new()),
        });
        listener.push(Box::new(FakeTcpConnection::new(
            Vec::new(),
            Arc::new(Mutex::new(Vec::new())),
            SocketAddr::from(([192, 0, 2, 14], 53004)),
        )));
        listener.push(Box::new(FakeTcpConnection::new(
            encode_frame(&wire_query(0x1357), MAX_DNS_WIRE_BYTES).unwrap(),
            Arc::new(Mutex::new(Vec::new())),
            SocketAddr::from(([192, 0, 2, 15], 53005)),
        )));

        let inbound = adapter(listener)
            .receive(&Cancellation::new())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(inbound.request().context.meta.original_dns_id, Some(0x1357));
        assert_eq!(
            inbound.request().context.meta.connection_id,
            Some(crate::dns::ConnectionId(2))
        );
    }

    #[tokio::test]
    async fn skips_partial_frame_and_accepts_the_next_connection() {
        let listener = Arc::new(FakeTcpListener {
            connections: Mutex::new(VecDeque::new()),
        });
        listener.push(Box::new(FakeTcpConnection::new(
            vec![0, 3, 1],
            Arc::new(Mutex::new(Vec::new())),
            SocketAddr::from(([192, 0, 2, 16], 53006)),
        )));
        listener.push(Box::new(FakeTcpConnection::new(
            encode_frame(&wire_query(0x2468), MAX_DNS_WIRE_BYTES).unwrap(),
            Arc::new(Mutex::new(Vec::new())),
            SocketAddr::from(([192, 0, 2, 17], 53007)),
        )));

        let inbound = adapter(listener)
            .receive(&Cancellation::new())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(inbound.request().context.meta.original_dns_id, Some(0x2468));
        assert_eq!(
            inbound.request().context.meta.connection_id,
            Some(crate::dns::ConnectionId(2))
        );
    }

    #[tokio::test]
    async fn cancellation_stops_accept_without_error() {
        let listener = Arc::new(FakeTcpListener {
            connections: Mutex::new(VecDeque::new()),
        });
        let cancellation = Cancellation::new();
        cancellation.cancel(crate::dns::CancelReason::Shutdown);

        assert!(
            adapter(listener)
                .receive(&cancellation)
                .await
                .unwrap()
                .is_none()
        );
    }
}
