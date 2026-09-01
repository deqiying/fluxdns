//! Tokio outbound TCP dialer 与 stream adapter。
//!
//! 该 adapter 只负责连接一个已经解析好的 proxy `SocketAddr`，不承担
//! proxy hostname 解析、SOCKS 协议或连接池管理。

use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, lookup_host};

use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::effects::{
    OutboundAddressResolver, OutboundDialer, OutboundStream, TcpReadChunkResult, TcpReadResult,
};
use crate::ports::{PortError, PortErrorClass, PortFuture};

const MAX_PROXY_ADDRESSES: usize = 16;

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioOutboundDialer;

impl TokioOutboundDialer {
    pub const fn new() -> Self {
        Self
    }
}

impl OutboundDialer for TokioOutboundDialer {
    fn connect<'a>(
        &'a self,
        target: SocketAddr,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Box<dyn OutboundStream>, PortError>> {
        Box::pin(async move {
            let stream = await_io(
                TcpStream::connect(target),
                deadline,
                cancellation,
                "outbound.tcp_connect",
            )
            .await?;
            Ok(Box::new(TokioOutboundStream { stream }) as Box<dyn OutboundStream>)
        })
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TokioOutboundAddressResolver;

impl TokioOutboundAddressResolver {
    pub const fn new() -> Self {
        Self
    }
}

impl OutboundAddressResolver for TokioOutboundAddressResolver {
    fn resolve<'a>(
        &'a self,
        host: &'a str,
        port: u16,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Vec<SocketAddr>, PortError>> {
        Box::pin(async move {
            if host.is_empty() || host.bytes().any(|byte| matches!(byte, b'\r' | b'\n')) {
                return Err(PortError::new(
                    PortErrorClass::InvalidInput,
                    "outbound.proxy_resolve",
                ));
            }
            if port == 0 {
                return Err(PortError::new(
                    PortErrorClass::InvalidInput,
                    "outbound.proxy_resolve",
                ));
            }
            let mut addresses = await_io(
                lookup_host((host, port)),
                deadline,
                cancellation,
                "outbound.proxy_resolve",
            )
            .await?;
            let addresses: Vec<_> = addresses.by_ref().take(MAX_PROXY_ADDRESSES + 1).collect();
            if addresses.len() > MAX_PROXY_ADDRESSES {
                return Err(PortError::new(
                    PortErrorClass::ResourceExhausted,
                    "outbound.proxy_resolve",
                ));
            }
            if addresses.is_empty() {
                return Err(PortError::new(
                    PortErrorClass::Unavailable,
                    "outbound.proxy_resolve",
                ));
            }
            Ok(addresses)
        })
    }
}

struct TokioOutboundStream {
    stream: TcpStream,
}

impl OutboundStream for TokioOutboundStream {
    fn read_exact<'a>(
        &'a mut self,
        length: usize,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<TcpReadResult, PortError>> {
        Box::pin(async move {
            if length == 0 {
                return Ok(TcpReadResult::Complete(Vec::new()));
            }
            let mut bytes = vec![0; length];
            let mut offset = 0;
            while offset < length {
                let count = await_io(
                    self.stream.read(&mut bytes[offset..]),
                    deadline,
                    cancellation,
                    "outbound.tcp_read",
                )
                .await?;
                if count == 0 {
                    return Ok(TcpReadResult::CleanEof);
                }
                offset += count;
            }
            Ok(TcpReadResult::Complete(bytes))
        })
    }

    fn read_chunk<'a>(
        &'a mut self,
        max_bytes: usize,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<TcpReadChunkResult, PortError>> {
        Box::pin(async move {
            if max_bytes == 0 {
                return Err(PortError::new(
                    PortErrorClass::InvalidInput,
                    "outbound.tcp_read_chunk",
                ));
            }
            let mut bytes = vec![0; max_bytes];
            let count = await_io(
                self.stream.read(&mut bytes),
                deadline,
                cancellation,
                "outbound.tcp_read_chunk",
            )
            .await?;
            if count == 0 {
                return Ok(TcpReadChunkResult::CleanEof);
            }
            bytes.truncate(count);
            Ok(TcpReadChunkResult::Data(bytes))
        })
    }

    fn write_all<'a>(
        &'a mut self,
        payload: Vec<u8>,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<(), PortError>> {
        Box::pin(async move {
            await_io(
                self.stream.write_all(&payload),
                deadline,
                cancellation,
                "outbound.tcp_write",
            )
            .await
        })
    }

    fn shutdown(&mut self) -> PortFuture<'_, Result<(), PortError>> {
        Box::pin(async move {
            self.stream
                .shutdown()
                .await
                .map_err(|_| PortError::new(PortErrorClass::Unavailable, "outbound.tcp_shutdown"))
        })
    }
}

async fn await_io<F, T>(
    future: F,
    deadline: Deadline,
    cancellation: &Cancellation,
    operation: &'static str,
) -> Result<T, PortError>
where
    F: std::future::Future<Output = io::Result<T>> + Send,
    T: Send,
{
    if cancellation.is_cancelled() {
        return Err(PortError::new(
            PortErrorClass::Cancelled(
                cancellation
                    .reason()
                    .unwrap_or(CancelReason::UpstreamCancelled),
            ),
            operation,
        ));
    }
    if deadline.is_expired(Instant::now()) {
        return Err(PortError::new(PortErrorClass::Timeout, operation));
    }
    tokio::select! {
        result = tokio::time::timeout(deadline.remaining(Instant::now()), future) => {
            match result {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(_)) => Err(PortError::new(PortErrorClass::Unavailable, operation)),
                Err(_) => Err(PortError::new(PortErrorClass::Timeout, operation)),
            }
        }
        _ = cancellation.cancelled() => Err(PortError::new(
            PortErrorClass::Cancelled(cancellation.reason().unwrap_or(CancelReason::UpstreamCancelled)),
            operation,
        )),
    }
}
