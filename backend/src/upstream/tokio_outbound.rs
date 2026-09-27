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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;

    fn deadline() -> Deadline {
        Deadline::new(Instant::now() + Duration::from_secs(5))
    }

    // V3-O01：真实 dialer/stream 的地址、分段读取、双向写入及 EOF 契约。
    #[tokio::test]
    async fn contract_v3_loopback_outbound_preserves_bytes_bounds_and_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let cancellation = Cancellation::new();
        let mut client = TokioOutboundDialer
            .connect(address, deadline(), &cancellation)
            .await
            .unwrap();
        let (mut server, _) = listener.accept().await.unwrap();
        assert_eq!(server.local_addr().unwrap(), address);
        client
            .write_all(vec![1, 2, 3], deadline(), &cancellation)
            .await
            .unwrap();
        let mut bytes = [0; 3];
        tokio::time::timeout(Duration::from_secs(5), server.read_exact(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, [1, 2, 3]);
        server.write_all(&[4, 5, 6]).await.unwrap();
        let chunk = client
            .read_chunk(1, deadline(), &cancellation)
            .await
            .unwrap();
        assert_eq!(chunk, TcpReadChunkResult::Data(vec![4]));
        assert_eq!(
            client
                .read_exact(2, deadline(), &cancellation)
                .await
                .unwrap(),
            TcpReadResult::Complete(vec![5, 6])
        );
        server.shutdown().await.unwrap();
        assert_eq!(
            client
                .read_chunk(8, deadline(), &cancellation)
                .await
                .unwrap(),
            TcpReadChunkResult::CleanEof
        );
        client.shutdown().await.unwrap();
    }

    // V3-O02：在真实 read 已 poll 为 Pending 后取消，不用先取消代替在途取消。
    #[tokio::test]
    async fn contract_v3_outbound_inflight_cancel_and_expired_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cancellation = Cancellation::new();
        let mut client = TokioOutboundDialer
            .connect(listener.local_addr().unwrap(), deadline(), &cancellation)
            .await
            .unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        let read = client.read_exact(1, deadline(), &cancellation);
        tokio::pin!(read);
        std::future::poll_fn(|cx| {
            assert!(read.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        cancellation.cancel(CancelReason::Shutdown);
        let error = tokio::time::timeout(Duration::from_secs(5), &mut read)
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(
            error.class(),
            PortErrorClass::Cancelled(CancelReason::Shutdown)
        ));
        assert_eq!(error.operation(), "outbound.tcp_read");

        let expired = Deadline::new(Instant::now());
        let fresh = Cancellation::new();
        let error = match TokioOutboundDialer
            .connect(listener.local_addr().unwrap(), expired, &fresh)
            .await
        {
            Ok(_) => panic!("expired connect must fail"),
            Err(error) => error,
        };
        assert!(matches!(error.class(), PortErrorClass::Timeout));
        for (host, port) in [("", 53), ("bad\r\nhost", 53), ("localhost", 0)] {
            let error = TokioOutboundAddressResolver
                .resolve(host, port, deadline(), &fresh)
                .await
                .unwrap_err();
            assert!(matches!(error.class(), PortErrorClass::InvalidInput));
        }
    }
}
