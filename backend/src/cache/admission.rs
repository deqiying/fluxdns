//! DNS 响应的缓存准入、TTL 和质量映射。

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::dns::{CanonicalResponse, ResponseClass, RuntimeRevision};
use crate::ports::cache::{
    CACHE_ENTRY_FORMAT_VERSION, CacheEntry, CacheQuality, CacheResponseClass,
    CacheUpstreamProvenance,
};

/// CacheStore 之外的准入参数；配置校验负责保证 failure TTL 为正数。
///
/// REFUSED 与空 Answer 固定缓存 10s 且不允许 stale，优先于下面的策略参数。
/// 其余应答按质量选窗口：完整应答使用 `optimistic_max_age`，带 Answer 的 NXDOMAIN 使用
/// `negative_optimistic_max_age`，SERVFAIL/TC 不设置 stale 窗口；lookup 再按当前池收紧。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CacheAdmissionPolicy {
    pub failure_ttl: Duration,
    pub optimistic_max_age: Option<Duration>,
    /// NODATA/NXDOMAIN fresh 生命周期上限；`None` 表示完全遵循 SOA 负 TTL。
    pub negative_ttl_max: Option<Duration>,
    /// NODATA/NXDOMAIN 过期后的乐观窗口；`None` 或零表示负应答不乐观返回。
    pub negative_optimistic_max_age: Option<Duration>,
}

impl CacheAdmissionPolicy {
    /// 仅设置失败 TTL 与完整应答乐观窗口；负应答不设上限也不乐观返回。
    pub const fn new(failure_ttl: Duration, optimistic_max_age: Option<Duration>) -> Self {
        Self {
            failure_ttl,
            optimistic_max_age,
            negative_ttl_max: None,
            negative_optimistic_max_age: None,
        }
    }

    /// 设置负应答的 TTL 上限与乐观窗口。
    pub const fn with_negative(
        mut self,
        negative_ttl_max: Option<Duration>,
        negative_optimistic_max_age: Option<Duration>,
    ) -> Self {
        self.negative_ttl_max = negative_ttl_max;
        self.negative_optimistic_max_age = negative_optimistic_max_age;
        self
    }
}

impl Default for CacheAdmissionPolicy {
    fn default() -> Self {
        Self::new(Duration::from_secs(5), None)
    }
}

/// 不应写入 response cache 的终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheAdmissionRejection {
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
    let short_ttl = response.short_cache_ttl();
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
            negative_ttl(&response, policy.failure_ttl, policy.negative_ttl_max),
        ),
        ResponseClass::NxDomain => (
            CacheResponseClass::NxDomain,
            CacheQuality::Negative,
            negative_ttl(&response, policy.failure_ttl, policy.negative_ttl_max),
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
        ResponseClass::Refused => (
            CacheResponseClass::Refused,
            CacheQuality::Failure,
            Ok(Duration::from_secs(10)),
        ),
        ResponseClass::Other(_) if short_ttl.is_some() => (
            CacheResponseClass::Other,
            CacheQuality::Failure,
            Ok(Duration::from_secs(10)),
        ),
        ResponseClass::Other(_) => {
            return Ok(CacheAdmissionOutcome::Rejected(
                CacheAdmissionRejection::OtherResponse,
            ));
        }
    };

    let ttl = match short_ttl.map(Ok).unwrap_or(ttl) {
        Ok(ttl) if !ttl.is_zero() => ttl,
        Ok(_) => {
            return Ok(CacheAdmissionOutcome::Rejected(
                CacheAdmissionRejection::ZeroTtl,
            ));
        }
        Err(rejection) => return Ok(CacheAdmissionOutcome::Rejected(rejection)),
    };
    let expires_at = now.checked_add(ttl).unwrap_or(now);
    let stale_window = if short_ttl.is_some() {
        // 短期拒绝/空结果到期即回源，不允许负缓存乐观窗口继续延长。
        None
    } else {
        match quality {
            CacheQuality::Complete => policy.optimistic_max_age,
            CacheQuality::Negative => policy.negative_optimistic_max_age,
            CacheQuality::Failure => None,
        }
    };
    let stale_until = stale_window
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

/// 负 TTL 依次取 SOA 负 TTL、RR 最小 TTL、failure TTL，再按配置上限截断。
fn negative_ttl(
    response: &CanonicalResponse,
    failure_ttl: Duration,
    negative_ttl_max: Option<Duration>,
) -> Result<Duration, CacheAdmissionRejection> {
    let ttl = response
        .ttl()
        .negative_ttl
        .or(response.ttl().min_ttl)
        .map(|seconds| Duration::from_secs(u64::from(seconds)))
        .or_else(|| (!failure_ttl.is_zero()).then_some(failure_ttl))
        .ok_or(CacheAdmissionRejection::MissingTtl)?;
    Ok(negative_ttl_max
        .filter(|max| !max.is_zero())
        .map_or(ttl, |max| ttl.min(max)))
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{
        Name, RData, Record, RecordType,
        rdata::{A, SOA},
    };

    use crate::dns::{CanonicalQuery, CanonicalResponse, DnsMessageId, RuntimeRevision};
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

    /// 构造 authority 段带 SOA 的负应答；SOA RR TTL 与 MINIMUM 取较小值作为负 TTL。
    fn negative_with_soa(code: ResponseCode, soa_ttl: u32, minimum: u32) -> CanonicalResponse {
        let query = query();
        let mut message = CanonicalResponse::empty_response(&query, code)
            .unwrap()
            .into_message();
        message.add_authority(Record::from_rdata(
            Name::from_str("example.com.").unwrap(),
            soa_ttl,
            RData::SOA(SOA::new(
                Name::from_str("ns.example.com.").unwrap(),
                Name::from_str("dns.example.com.").unwrap(),
                1,
                10_000,
                2_400,
                604_800,
                minimum,
            )),
        ));
        CanonicalResponse::from_message(message, &query, DnsMessageId::new(0)).unwrap()
    }

    fn upstream() -> CacheUpstreamProvenance {
        CacheUpstreamProvenance::direct_from_validated_config_id("test-upstream").unwrap()
    }

    fn accepted(
        policy: CacheAdmissionPolicy,
        response: CanonicalResponse,
        now: Instant,
    ) -> Arc<crate::ports::cache::CacheEntry> {
        match admit_response(
            policy,
            Arc::new(response),
            upstream(),
            now,
            RuntimeRevision(1),
        )
        .unwrap()
        {
            CacheAdmissionOutcome::Accepted(entry) => entry,
            CacheAdmissionOutcome::Rejected(rejection) => {
                panic!("expected accepted, got {rejection:?}")
            }
        }
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
    fn empty_negative_uses_ten_seconds_without_stale_window() {
        let now = Instant::now();
        let policy =
            CacheAdmissionPolicy::new(Duration::from_secs(7), Some(Duration::from_secs(60)))
                .with_negative(None, Some(Duration::from_secs(20)));
        let entry = accepted(policy, response_with_code(ResponseCode::NXDomain), now);
        assert_eq!(entry.response_class, CacheResponseClass::NxDomain);
        assert_eq!(entry.quality, CacheQuality::Negative);
        assert_eq!(entry.expires_at, now + Duration::from_secs(10));
        assert_eq!(entry.stale_until, None);
    }

    #[test]
    fn negative_without_negative_window_is_never_stale() {
        let now = Instant::now();
        let policy =
            CacheAdmissionPolicy::new(Duration::from_secs(7), Some(Duration::from_secs(60)));
        let entry = accepted(policy, response_with_code(ResponseCode::NoError), now);
        assert_eq!(entry.response_class, CacheResponseClass::NoData);
        assert_eq!(entry.stale_until, None);
    }

    #[test]
    fn empty_response_ttl_overrides_soa_and_config_without_affecting_positive() {
        let now = Instant::now();
        let policy = CacheAdmissionPolicy::new(Duration::from_secs(5), None)
            .with_negative(Some(Duration::from_secs(3)), Some(Duration::from_secs(300)));
        for code in [ResponseCode::NoError, ResponseCode::NXDomain] {
            for origin_ttl in [0, 1, 60, 1800] {
                let response = negative_with_soa(code, origin_ttl, origin_ttl);
                let checksum = canonical_checksum(&response).unwrap();
                let entry = accepted(policy, response, now);
                assert_eq!(entry.expires_at, now + Duration::from_secs(10));
                assert_eq!(entry.stale_until, None);
                assert_eq!(entry.response.ttl().negative_ttl, Some(origin_ttl));
                assert_eq!(entry.checksum, checksum, "缓存保留 origin response");
            }
        }

        // 完整应答不受负 TTL 上限影响。
        let query = query();
        let long_answer = Record::from_rdata(
            Name::from_str("example.com.").unwrap(),
            3600,
            RData::A(A(Ipv4Addr::new(192, 0, 2, 1))),
        );
        let positive = accepted(
            policy,
            CanonicalResponse::response_with_answers(&query, [long_answer]).unwrap(),
            now,
        );
        assert_eq!(positive.expires_at, now + Duration::from_secs(3600));
    }

    #[test]
    fn failure_classes_never_get_stale_window_and_positive_keeps_its_window() {
        let now = Instant::now();
        let policy =
            CacheAdmissionPolicy::new(Duration::from_secs(5), Some(Duration::from_secs(60)))
                .with_negative(
                    Some(Duration::from_secs(300)),
                    Some(Duration::from_secs(30)),
                );
        let servfail = accepted(policy, response_with_code(ResponseCode::ServFail), now);
        assert_eq!(servfail.quality, CacheQuality::Failure);
        assert_eq!(servfail.stale_until, None);
        let positive = accepted(policy, positive_response(), now);
        assert_eq!(positive.stale_until, Some(now + Duration::from_secs(90)));
    }

    #[test]
    fn refuses_zero_ttl_positive_responses() {
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
    fn admits_empty_response_codes_and_refused_with_answers_for_ten_seconds() {
        let now = Instant::now();
        let policy =
            CacheAdmissionPolicy::new(Duration::from_secs(100), Some(Duration::from_secs(60)));
        for code in [
            ResponseCode::NoError,
            ResponseCode::NXDomain,
            ResponseCode::Refused,
            ResponseCode::ServFail,
            ResponseCode::FormErr,
            ResponseCode::NotImp,
        ] {
            for truncated in [false, true] {
                let mut message = response_with_code(code).into_message();
                message.metadata.truncation = truncated;
                let response =
                    CanonicalResponse::from_message(message, &query(), DnsMessageId::new(0))
                        .unwrap();
                let entry = accepted(policy, response, now);
                assert_eq!(entry.expires_at, now + Duration::from_secs(10));
                assert_eq!(entry.stale_until, None);
                assert_eq!(entry.response.as_message().metadata.response_code, code);
                assert_eq!(entry.response.as_message().metadata.truncation, truncated);
            }
        }
        let mut refused = positive_response().into_message();
        refused.metadata.response_code = ResponseCode::Refused;
        let entry = accepted(
            policy,
            CanonicalResponse::from_message(refused, &query(), DnsMessageId::new(0)).unwrap(),
            now,
        );
        assert_eq!(entry.response_class, CacheResponseClass::Refused);
        assert_eq!(entry.quality, CacheQuality::Failure);
        assert_eq!(entry.expires_at, now + Duration::from_secs(10));
        assert_eq!(entry.stale_until, None);
    }

    #[test]
    fn nonempty_negative_and_failure_keep_existing_policies() {
        let now = Instant::now();
        let policy = CacheAdmissionPolicy::new(Duration::from_secs(7), None)
            .with_negative(Some(Duration::from_secs(20)), Some(Duration::from_secs(40)));
        for code in [ResponseCode::NXDomain, ResponseCode::ServFail] {
            let mut message = positive_response().into_message();
            message.metadata.response_code = code;
            let entry = accepted(
                policy,
                CanonicalResponse::from_message(message, &query(), DnsMessageId::new(0)).unwrap(),
                now,
            );
            let (ttl, stale) = if code == ResponseCode::NXDomain {
                (20, Some(now + Duration::from_secs(60)))
            } else {
                (7, None)
            };
            assert_eq!(entry.expires_at, now + Duration::from_secs(ttl));
            assert_eq!(entry.stale_until, stale);
        }
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
