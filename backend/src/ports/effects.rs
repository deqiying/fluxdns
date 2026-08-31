//! 时间、资源、secret 与 socket 准备等受约束副作用。

use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use crate::dns::{Cancellation, Deadline};

use super::{PortError, PortFuture};

pub trait Clock: Send + Sync {
    fn monotonic_now(&self) -> Instant;

    fn utc_now(&self) -> SystemTime;

    fn sleep_until(&self, deadline: Deadline) -> PortFuture<'_, ()>;
}

#[derive(Clone)]
pub struct ResourceLocation(Arc<str>);

impl ResourceLocation {
    pub fn new(value: impl Into<Arc<str>>) -> Result<Self, PortError> {
        let value = value.into();
        if value.is_empty() {
            return Err(PortError::new(
                super::PortErrorClass::InvalidInput,
                "resource_location",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ResourceLocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceLocation")
            .field("byte_len", &self.0.len())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProxyProfileId(pub Arc<str>);

#[derive(Clone)]
pub struct ResourceFetchRequest {
    pub location: ResourceLocation,
    pub proxy_profile: Option<ProxyProfileId>,
    pub max_bytes: usize,
    pub deadline: Deadline,
    pub cancellation: Cancellation,
}

impl fmt::Debug for ResourceFetchRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceFetchRequest")
            .field("location_byte_len", &self.location.0.len())
            .field("has_proxy_profile", &self.proxy_profile.is_some())
            .field("max_bytes", &self.max_bytes)
            .field("deadline", &self.deadline)
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish()
    }
}

#[derive(Clone)]
pub struct ResourceFetchResult {
    pub body: Arc<[u8]>,
    pub checksum: u64,
    pub modified_at: Option<SystemTime>,
}

impl fmt::Debug for ResourceFetchResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResourceFetchResult")
            .field("body_len", &self.body.len())
            .field("checksum", &self.checksum)
            .field("modified_at", &self.modified_at)
            .finish()
    }
}

pub trait ResourceFetcher: Send + Sync {
    fn fetch(
        &self,
        request: ResourceFetchRequest,
    ) -> PortFuture<'_, Result<ResourceFetchResult, PortError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecretReference {
    Environment(Arc<str>),
    File(PathBuf),
}

/// secret 值的 `Debug`/`Display` 固定脱敏，且不实现序列化 trait。
pub struct SecretValue(Box<[u8]>);

impl SecretValue {
    pub fn new(value: impl Into<Box<[u8]>>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue([REDACTED])")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

pub trait SecretProvider: Send + Sync {
    fn resolve<'a>(
        &'a self,
        reference: &'a SecretReference,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<SecretValue, PortError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketKind {
    Udp,
    Tcp,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketSpec {
    pub kind: SocketKind,
    pub address: SocketAddr,
    pub reuse_port: bool,
    pub v6_only: bool,
}

/// Runtime `BindPlan` 提交前的未激活 socket，不暴露 socket2/Tokio 类型。
pub trait PreparedSocket: Send {
    fn local_addr(&self) -> Result<SocketAddr, PortError>;

    fn activate(self: Box<Self>) -> Result<Box<dyn ActivatedSocket>, PortError>;
}

pub trait ActivatedSocket: Send + Sync {
    fn local_addr(&self) -> Result<SocketAddr, PortError>;
}

pub trait SocketFactory: Send + Sync {
    fn prepare<'a>(
        &'a self,
        spec: SocketSpec,
        deadline: Deadline,
        cancellation: &'a Cancellation,
    ) -> PortFuture<'a, Result<Box<dyn PreparedSocket>, PortError>>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use crate::dns::{Cancellation, Deadline};

    use super::{
        ProxyProfileId, ResourceFetchRequest, ResourceFetchResult, ResourceLocation, SecretValue,
    };

    #[test]
    fn secret_value_debug_and_display_are_redacted() {
        let secret = SecretValue::new(Vec::from("do-not-leak").into_boxed_slice());

        assert_eq!(format!("{secret}"), "[REDACTED]");
        assert_eq!(format!("{secret:?}"), "SecretValue([REDACTED])");
        assert!(!format!("{secret:?}").contains("do-not-leak"));
    }

    #[test]
    fn resource_fetch_debug_only_contains_safe_metadata() {
        let location = ResourceLocation::new(
            "https://user:password@rules.example/private?access_token=do-not-log-this",
        )
        .unwrap();
        let request = ResourceFetchRequest {
            location: location.clone(),
            proxy_profile: Some(ProxyProfileId(Arc::from("private-proxy"))),
            max_bytes: 8 * 1024,
            deadline: Deadline::new(Instant::now()),
            cancellation: Cancellation::new(),
        };
        let result = ResourceFetchResult {
            body: Arc::from(&b"domain example.com\nprivate-rule-body"[..]),
            checksum: 42,
            modified_at: None,
        };

        let location_debug = format!("{location:?}");
        let request_debug = format!("{request:?}");
        let result_debug = format!("{result:?}");

        for secret in [
            "user",
            "password",
            "rules.example",
            "access_token",
            "do-not-log-this",
            "private-proxy",
            "example.com",
            "private-rule-body",
        ] {
            assert!(!location_debug.contains(secret));
            assert!(!request_debug.contains(secret));
            assert!(!result_debug.contains(secret));
        }
        assert!(location_debug.contains("byte_len"));
        assert!(request_debug.contains("location_byte_len"));
        assert!(request_debug.contains("has_proxy_profile: true"));
        assert!(result_debug.contains("body_len"));
        assert!(result_debug.contains("checksum: 42"));
    }
}
