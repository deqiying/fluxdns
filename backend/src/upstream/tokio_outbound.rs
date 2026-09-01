//! Tokio outbound TCP dialer 与 stream adapter。
//!
//! 该 adapter 只负责连接一个已经解析好的 proxy `SocketAddr`，不承担
//! proxy hostname 解析、SOCKS 协议或连接池管理。

use std::io;
use std::net::SocketAddr;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::dns::{CancelReason, Cancellation, Deadline};
use crate::ports::effects::{OutboundDialer, OutboundStream, TcpReadResult};
use crate::ports::{PortError, PortErrorClass, PortFuture};

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
