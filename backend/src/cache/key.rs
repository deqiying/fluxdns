//! Cache key 的稳定、无客户端明文编码。

use std::fmt;
use std::sync::Arc;

use crate::dns::{CacheCompatibilityKey, CanonicalQuery};
use crate::ports::cache::{CacheKey, CacheNamespace};

pub const CACHE_KEY_FORMAT_VERSION: u16 = 1;

/// 由 policy/resource 层产生的 opaque 32-byte fingerprint。
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CacheFingerprint([u8; 32]);

impl CacheFingerprint {
    pub const fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl fmt::Debug for CacheFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CacheFingerprint(REDACTED)")
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheKeyDimensions {
    pub policy: Option<CacheFingerprint>,
    pub target: Option<CacheFingerprint>,
    pub ecs: Option<CacheFingerprint>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheKeyError {
    QueryEncoding,
    ComponentTooLarge,
}

/// 构造包含所有会改变答案的 opaque 维度的 cache key。
pub fn build_cache_key(
    namespace: CacheNamespace,
    query: &CanonicalQuery,
    transport: CacheCompatibilityKey,
    dimensions: CacheKeyDimensions,
) -> Result<CacheKey, CacheKeyError> {
    let query_wire = query
        .as_message()
        .to_vec()
        .map_err(|_| CacheKeyError::QueryEncoding)?;
    let mut encoded = Vec::with_capacity(64 + query_wire.len());
    encoded.extend_from_slice(b"FDCK");
    encoded.extend_from_slice(&CACHE_KEY_FORMAT_VERSION.to_be_bytes());
    encode_namespace(&mut encoded, &namespace)?;
    encoded.extend_from_slice(&transport.value().to_be_bytes());
    encode_fingerprint(&mut encoded, dimensions.policy);
    encode_fingerprint(&mut encoded, dimensions.target);
    encode_fingerprint(&mut encoded, dimensions.ecs);
    encode_component(&mut encoded, &query_wire)?;

    Ok(CacheKey {
        namespace,
        encoded: Arc::from(encoded),
        format_version: CACHE_KEY_FORMAT_VERSION,
    })
}

fn encode_namespace(output: &mut Vec<u8>, namespace: &CacheNamespace) -> Result<(), CacheKeyError> {
    match namespace {
        CacheNamespace::Global => output.push(0),
        CacheNamespace::Strategy(strategy) => {
            output.push(1);
            encode_component(output, strategy.as_bytes())?;
        }
        CacheNamespace::ClientStrategy {
            client_digest,
            strategy,
        } => {
            output.push(2);
            output.extend_from_slice(&client_digest.as_bytes());
            encode_component(output, strategy.as_bytes())?;
        }
    }
    Ok(())
}

fn encode_fingerprint(output: &mut Vec<u8>, fingerprint: Option<CacheFingerprint>) {
    match fingerprint {
        Some(fingerprint) => {
            output.push(1);
            output.extend_from_slice(&fingerprint.as_bytes());
        }
        None => output.push(0),
    }
}

fn encode_component(output: &mut Vec<u8>, component: &[u8]) -> Result<(), CacheKeyError> {
    let length = u32::try_from(component.len()).map_err(|_| CacheKeyError::ComponentTooLarge)?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(component);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    use crate::dns::{CacheCompatibilityKey, CanonicalQuery};
    use crate::ports::cache::{CacheNamespace, CacheStrategyId, ClientCacheDigest};

    use super::{CACHE_KEY_FORMAT_VERSION, CacheFingerprint, CacheKeyDimensions, build_cache_key};

    fn query(id: u16) -> CanonicalQuery {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("Example.COM.").unwrap(),
            RecordType::A,
        ));
        CanonicalQuery::from_message(message).unwrap()
    }

    #[test]
    fn canonical_query_ids_do_not_change_the_key() {
        let first = build_cache_key(
            CacheNamespace::Global,
            &query(1),
            CacheCompatibilityKey(7),
            CacheKeyDimensions::default(),
        )
        .unwrap();
        let second = build_cache_key(
            CacheNamespace::Global,
            &query(65535),
            CacheCompatibilityKey(7),
            CacheKeyDimensions::default(),
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.format_version, CACHE_KEY_FORMAT_VERSION);
    }

    #[test]
    fn namespace_transport_and_opaque_dimensions_are_keyed() {
        let base = build_cache_key(
            CacheNamespace::Global,
            &query(1),
            CacheCompatibilityKey(1),
            CacheKeyDimensions::default(),
        )
        .unwrap();
        let namespace = CacheNamespace::ClientStrategy {
            client_digest: ClientCacheDigest::from_digest([0x11; 32]),
            strategy: CacheStrategyId::from_validated_config_id("route-a").unwrap(),
        };
        let client = build_cache_key(
            namespace,
            &query(1),
            CacheCompatibilityKey(1),
            CacheKeyDimensions {
                policy: Some(CacheFingerprint::from_digest([0x22; 32])),
                target: Some(CacheFingerprint::from_digest([0x33; 32])),
                ecs: Some(CacheFingerprint::from_digest([0x44; 32])),
            },
        )
        .unwrap();
        let other_transport = build_cache_key(
            CacheNamespace::Global,
            &query(1),
            CacheCompatibilityKey(2),
            CacheKeyDimensions::default(),
        )
        .unwrap();
        assert_ne!(base, client);
        assert_ne!(base, other_transport);
    }

    #[test]
    fn key_debug_redacts_encoded_material_and_fingerprints() {
        let key = build_cache_key(
            CacheNamespace::Global,
            &query(1),
            CacheCompatibilityKey(1),
            CacheKeyDimensions {
                policy: Some(CacheFingerprint::from_digest([0xAA; 32])),
                ..CacheKeyDimensions::default()
            },
        )
        .unwrap();
        let debug = format!("{key:?}");
        assert!(!debug.contains("Example"));
        assert!(!debug.contains("aa"));
        assert!(debug.contains("encoded_len"));
    }
}
