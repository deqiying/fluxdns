//! DNS 响应的缓存准入、TTL 和质量映射。

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dns::{CanonicalResponse, ResponseClass, RuntimeRevision};
use crate::ports::cache::{
    CACHE_ENTRY_FORMAT_VERSION, CacheEntry, CacheQuality, CacheResponseClass,
    CacheUpstreamProvenance,
};

/// CacheStore 之外的准入参数；配置校验负责保证 failure TTL 为正数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheAdmissionPolicy {
    pub failure_ttl: Duration,
    pub optimistic_max_age: Option<Duration>,
}

impl CacheAdmissionPolicy {
    pub const fn new(failure_ttl: Duration, optimistic_max_age: Option<Duration>) -> Self {
        Self {
            failure_ttl,
            optimistic_max_age,
        }
    }
}

impl Default for CacheAdmissionPolicy {
    fn default() -> Self {
        Self {
            failure_ttl: Duration::from_secs(5),
            optimistic_max_age: None,
        }
    }
}

/// 不应写入 response cache 的终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheAdmissionRejection {
    Refused,
    OtherResponse,
    MissingTtl,
    ZeroTtl,
}

/// canonical response 编码失败；该错误不是 DNS response failure。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheAdmissionError {
    ResponseEncoding,
}

#[derive(Clone, Debug)]
pub enum CacheAdmissionOutcome {
    Accepted(Arc<CacheEntry>),
    Rejected(CacheAdmissionRejection),
}

/// 根据 canonical response 计算稳定的无密钥校验摘要。
pub fn canonical_checksum(response: &CanonicalResponse) -> Result<u64, CacheAdmissionError> {
    let wire = response
        .as_message()
        .to_vec()
        .map_err(|_| CacheAdmissionError::ResponseEncoding)?;
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in wire {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3_u64);
    }
    Ok(hash)
}

/// 将已验证的 DNS response 转为可写入 CacheStore 的 entry。
pub fn admit_response(
    policy: CacheAdmissionPolicy,
    response: Arc<CanonicalResponse>,
    upstream: CacheUpstreamProvenance,
    now: Instant,
    producer_revision: RuntimeRevision,
) -> Result<CacheAdmissionOutcome, CacheAdmissionError> {
    let (response_class, quality, ttl) = match response.class() {
        ResponseClass::Positive => (
            CacheResponseClass::NoError,
            CacheQuality::Complete,
            response
                .ttl()
                .min_ttl
                .map(|seconds| Duration::from_secs(u64::from(seconds)))
                .ok_or(CacheAdmissionRejection::MissingTtl),
        ),
        ResponseClass::NoData => (
            CacheResponseClass::NoData,
            CacheQuality::Negative,
            negative_ttl(&response, policy.failure_ttl),
        ),
        ResponseClass::NxDomain => (
            CacheResponseClass::NxDomain,
            CacheQuality::Negative,
            negative_ttl(&response, policy.failure_ttl),
        ),
        ResponseClass::ServFail => (
            CacheResponseClass::ServFail,
            CacheQuality::Failure,
            Ok(policy.failure_ttl),
        ),
        ResponseClass::Truncated => (
            CacheResponseClass::Truncated,
            CacheQuality::Failure,
            Ok(policy.failure_ttl),
        ),
        ResponseClass::Refused => {
            return Ok(CacheAdmissionOutcome::Rejected(
                CacheAdmissionRejection::Refused,
            ));
        }
        ResponseClass::Other(_) => {
            return Ok(CacheAdmissionOutcome::Rejected(
                CacheAdmissionRejection::OtherResponse,
            ));
        }
    };

    let ttl = match ttl {
        Ok(ttl) if !ttl.is_zero() => ttl,
        Ok(_) => {
            return Ok(CacheAdmissionOutcome::Rejected(
                CacheAdmissionRejection::ZeroTtl,
            ));
        }
        Err(rejection) => return Ok(CacheAdmissionOutcome::Rejected(rejection)),
    };
    let expires_at = now.checked_add(ttl).unwrap_or(now);
    let stale_until = policy
        .optimistic_max_age
        .filter(|max_age| !max_age.is_zero())
        .and_then(|max_age| expires_at.checked_add(max_age));
    let checksum = canonical_checksum(response.as_ref())?;

    Ok(CacheAdmissionOutcome::Accepted(Arc::new(CacheEntry {
        response,
        upstream,
        inserted_at: now,
        expires_at,
        stale_until,
        response_class,
        producer_revision,
        quality,
        checksum,
        format_version: CACHE_ENTRY_FORMAT_VERSION,
    })))
}

fn negative_ttl(
    response: &CanonicalResponse,
    failure_ttl: Duration,
) -> Result<Duration, CacheAdmissionRejection> {
    response
        .ttl()
        .negative_ttl
        .or(response.ttl().min_ttl)
        .map(|seconds| Duration::from_secs(u64::from(seconds)))
        .or_else(|| (!failure_ttl.is_zero()).then_some(failure_ttl))
        .ok_or(CacheAdmissionRejection::MissingTtl)
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RData, Record, RecordType, rdata::A};

    use crate::dns::{CanonicalQuery, CanonicalResponse, RuntimeRevision};
    use crate::ports::cache::{CacheQuality, CacheResponseClass, CacheUpstreamProvenance};

    use super::{
        CacheAdmissionOutcome, CacheAdmissionPolicy, CacheAdmissionRejection, admit_response,
        canonical_checksum,
    };

    fn query() -> CanonicalQuery {
        let mut message = Message::new(1, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("example.com.").unwrap(),
            RecordType::A,
        ));
        CanonicalQuery::from_message(message).unwrap()
    }

    fn response_with_code(code: ResponseCode) -> CanonicalResponse {
        CanonicalResponse::empty_response(&query(), code).unwrap()
    }

    fn positive_response() -> CanonicalResponse {
        let query = query();
        let answer = Record::from_rdata(
            Name::from_str("example.com.").unwrap(),
            30,
            RData::A(A(Ipv4Addr::new(192, 0, 2, 1))),
        );
        CanonicalResponse::response_with_answers(&query, [answer]).unwrap()
    }

    fn upstream() -> CacheUpstreamProvenance {
        CacheUpstreamProvenance::direct_from_validated_config_id("test-upstream").unwrap()
    }

    #[test]
    fn admits_positive_with_origin_ttl_and_checksum() {
        let now = Instant::now();
        let response = Arc::new(positive_response());
        let checksum = canonical_checksum(response.as_ref()).unwrap();
        let outcome = admit_response(
            CacheAdmissionPolicy::default(),
            response,
            upstream(),
            now,
            RuntimeRevision(7),
        )
        .unwrap();
        let CacheAdmissionOutcome::Accepted(entry) = outcome else {
            panic!("expected accepted response");
        };
        assert_eq!(entry.response_class, CacheResponseClass::NoError);
        assert_eq!(entry.quality, CacheQuality::Complete);
        assert_eq!(entry.expires_at, now + Duration::from_secs(30));
        assert_eq!(entry.checksum, checksum);
        assert_eq!(entry.producer_revision, RuntimeRevision(7));
    }

    #[test]
    fn uses_failure_ttl_for_negative_and_sets_stale_window() {
        let now = Instant::now();
        let outcome = admit_response(
            CacheAdmissionPolicy::new(Duration::from_secs(7), Some(Duration::from_secs(20))),
            Arc::new(response_with_code(ResponseCode::NXDomain)),
            upstream(),
            now,
            RuntimeRevision(1),
        )
        .unwrap();
        let CacheAdmissionOutcome::Accepted(entry) = outcome else {
            panic!("expected accepted response");
        };
        assert_eq!(entry.response_class, CacheResponseClass::NxDomain);
        assert_eq!(entry.quality, CacheQuality::Negative);
        assert_eq!(entry.expires_at, now + Duration::from_secs(7));
        assert_eq!(entry.stale_until, Some(now + Duration::from_secs(27)));
    }

    #[test]
    fn refuses_refused_and_zero_ttl_responses() {
        let refused = admit_response(
            CacheAdmissionPolicy::default(),
            Arc::new(response_with_code(ResponseCode::Refused)),
            upstream(),
            Instant::now(),
            RuntimeRevision(1),
        )
        .unwrap();
        assert!(matches!(
            refused,
            CacheAdmissionOutcome::Rejected(CacheAdmissionRejection::Refused)
        ));

        let query = query();
        let answer = Record::from_rdata(
            Name::from_str("example.com.").unwrap(),
            0,
            RData::A(A(Ipv4Addr::new(192, 0, 2, 1))),
        );
        let zero_ttl = admit_response(
            CacheAdmissionPolicy::default(),
            Arc::new(CanonicalResponse::response_with_answers(&query, [answer]).unwrap()),
            upstream(),
            Instant::now(),
            RuntimeRevision(1),
        )
        .unwrap();
        assert!(matches!(
            zero_ttl,
            CacheAdmissionOutcome::Rejected(CacheAdmissionRejection::ZeroTtl)
        ));
    }

    #[test]
    fn checksum_is_stable_for_same_canonical_response() {
        let first = positive_response();
        let second = positive_response();
        assert_eq!(
            canonical_checksum(&first).unwrap(),
            canonical_checksum(&second).unwrap()
        );
    }
}
