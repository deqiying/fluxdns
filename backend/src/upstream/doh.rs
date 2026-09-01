//! DoH 上游 HTTP 响应验证。
//!
//! 本模块不建立 HTTP/TLS 连接，只把已读取的 HTTP 响应转换为稳定的
//! UpstreamOutcome，并校验 DNS wire 数据。

use hickory_proto::op::Message;

use crate::dns::{
    CancelReason, Cancellation, CanonicalMessageError, CanonicalQuery, CanonicalResponse,
    DnsMessageId,
};
use crate::ports::exchange::{
    ConnectorId, TransportFailure, TransportFailureClass, UpstreamOutcome,
};

/// DoH 响应 body 接受的最大 DNS wire 长度。
pub const MAX_DOH_RESPONSE_BODY_BYTES: usize = u16::MAX as usize;

/// 已读取的 HTTP response envelope。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DohHttpResponse<'a> {
    pub status: u16,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
}

impl<'a> DohHttpResponse<'a> {
    pub const fn new(status: u16, content_type: Option<&'a str>, body: &'a [u8]) -> Self {
        Self {
            status,
            content_type,
            body,
        }
    }
}

/// 校验一个 DoH HTTP response 并生成 upstream outcome。
///
/// 校验顺序为取消、HTTP 状态、媒体类型、body 大小，以及 DNS wire/request 关联。
/// 拒绝结果只暴露稳定的 TransportFailureClass，不包含 URL、body 或原始解析文本。
pub fn validate_response(
    connector: &ConnectorId,
    query: &CanonicalQuery,
    expected_id: DnsMessageId,
    response: DohHttpResponse<'_>,
    cancellation: &Cancellation,
) -> UpstreamOutcome {
    if cancellation.is_cancelled() {
        return UpstreamOutcome::Cancelled(
            cancellation
                .reason()
                .unwrap_or(CancelReason::UpstreamCancelled),
        );
    }

    if !(200..300).contains(&response.status) {
        return failure(
            connector,
            TransportFailureClass::HttpStatus,
            is_retryable_status(response.status),
            Some("DoH response status was not successful"),
        );
    }

    if !is_dns_message_content_type(response.content_type) {
        return failure(
            connector,
            TransportFailureClass::MediaType,
            false,
            Some("DoH response media type was not application/dns-message"),
        );
    }

    if response.body.len() > MAX_DOH_RESPONSE_BODY_BYTES {
        return failure(
            connector,
            TransportFailureClass::BodyLimit,
            false,
            Some("DoH response body exceeded the DNS wire limit"),
        );
    }

    let message = match Message::from_vec(response.body) {
        Ok(message) => message,
        Err(_) => {
            return failure(
                connector,
                TransportFailureClass::Wire,
                false,
                Some("DoH response DNS wire was invalid"),
            );
        }
    };

    match CanonicalResponse::from_message(message, query, expected_id) {
        Ok(response) => UpstreamOutcome::Response(response),
        Err(error) => failure(
            connector,
            classify_canonical_error(&error),
            false,
            Some(canonical_error_context(&error)),
        ),
    }
}

fn is_dns_message_content_type(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/dns-message"))
}

fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429) || (500..600).contains(&status)
}

fn classify_canonical_error(error: &CanonicalMessageError) -> TransportFailureClass {
    match error {
        CanonicalMessageError::MessageIdMismatch { .. } => TransportFailureClass::ProtocolViolation,
        CanonicalMessageError::QuestionMismatch => TransportFailureClass::QuestionMismatch,
        CanonicalMessageError::UnexpectedMessageType { .. }
        | CanonicalMessageError::UnsupportedOpCode(_)
        | CanonicalMessageError::QuestionCount(_)
        | CanonicalMessageError::UnsupportedEdnsVersion(_) => {
            TransportFailureClass::ProtocolViolation
        }
    }
}

fn canonical_error_context(error: &CanonicalMessageError) -> &'static str {
    match error {
        CanonicalMessageError::MessageIdMismatch { .. } => "DoH response ID did not match request",
        CanonicalMessageError::QuestionMismatch => "DoH response question did not match request",
        CanonicalMessageError::UnexpectedMessageType { .. } => {
            "DoH response did not have the DNS response flag"
        }
        CanonicalMessageError::UnsupportedOpCode(_) => "DoH response used an unsupported opcode",
        CanonicalMessageError::QuestionCount(_) => "DoH response had an invalid question count",
        CanonicalMessageError::UnsupportedEdnsVersion(_) => {
            "DoH response used an unsupported EDNS version"
        }
    }
}

fn failure(
    connector: &ConnectorId,
    class: TransportFailureClass,
    retryable: bool,
    safe_context: Option<&'static str>,
) -> UpstreamOutcome {
    UpstreamOutcome::TransportFailure(TransportFailure {
        connector: connector.clone(),
        class,
        retryable,
        safe_context,
    })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hickory_proto::{
        op::{Message, MessageType, OpCode, Query, ResponseCode},
        rr::{Name, RecordType},
    };

    use crate::dns::{CancelReason, CanonicalQuery, DnsMessageId};
    use crate::ports::exchange::{TransportFailureClass, UpstreamOutcome};

    use super::{DohHttpResponse, MAX_DOH_RESPONSE_BODY_BYTES, validate_response};

    fn connector() -> crate::ports::exchange::ConnectorId {
        crate::ports::exchange::ConnectorId::new("resolver-a").unwrap()
    }

    fn query() -> CanonicalQuery {
        let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("Example.COM.").unwrap(),
            RecordType::A,
        ));
        CanonicalQuery::from_message(message).unwrap()
    }

    fn wire_response(query: &CanonicalQuery, id: u16, code: ResponseCode) -> Vec<u8> {
        let mut message = Message::new(id, MessageType::Response, OpCode::Query);
        message.metadata.response_code = code;
        message.add_query(Query::query(
            query.question().name().clone(),
            query.question().query_type(),
        ));
        message.to_vec().unwrap()
    }

    fn outcome(status: u16, content_type: Option<&str>, body: &[u8]) -> UpstreamOutcome {
        validate_response(
            &connector(),
            &query(),
            DnsMessageId::new(0x1234),
            DohHttpResponse::new(status, content_type, body),
            &crate::dns::Cancellation::new(),
        )
    }

    #[test]
    fn accepts_successful_dns_message_with_content_type_parameters() {
        let query = query();
        let body = wire_response(&query, 0x1234, ResponseCode::NoError);
        let result = validate_response(
            &connector(),
            &query,
            DnsMessageId::new(0x1234),
            DohHttpResponse::new(200, Some(" Application/DNS-Message; charset=binary"), &body),
            &crate::dns::Cancellation::new(),
        );

        assert!(matches!(result, UpstreamOutcome::Response(_)));
    }

    #[test]
    fn rejects_status_media_type_and_body_boundaries() {
        assert!(matches!(
            outcome(503, Some("application/dns-message"), &[]),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::HttpStatus && failure.retryable
        ));
        assert!(matches!(
            outcome(200, Some("application/json"), &[]),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::MediaType
        ));

        let body = vec![0_u8; MAX_DOH_RESPONSE_BODY_BYTES + 1];
        assert!(matches!(
            outcome(200, Some("application/dns-message"), &body),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::BodyLimit
        ));
    }

    #[test]
    fn rejects_invalid_wire_id_and_question_mismatch() {
        assert!(matches!(
            outcome(200, Some("application/dns-message"), &[0xff, 0x00]),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::Wire
        ));

        let expected_query = query();
        let mut query_wire = Message::new(0x1234, MessageType::Query, OpCode::Query);
        query_wire.add_query(Query::query(
            expected_query.question().name().clone(),
            expected_query.question().query_type(),
        ));
        let query_wire = query_wire.to_vec().unwrap();
        assert!(matches!(
            validate_response(
                &connector(),
                &expected_query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(200, Some("application/dns-message"), &query_wire),
                &crate::dns::Cancellation::new(),
            ),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::ProtocolViolation
        ));

        let body = wire_response(&expected_query, 0x4321, ResponseCode::NoError);
        assert!(matches!(
            validate_response(
                &connector(),
                &expected_query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(200, Some("application/dns-message"), &body),
                &crate::dns::Cancellation::new(),
            ),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::ProtocolViolation
        ));

        let other_query = {
            let mut message = Message::new(1, MessageType::Query, OpCode::Query);
            message.add_query(Query::query(
                Name::from_str("other.example.").unwrap(),
                RecordType::A,
            ));
            CanonicalQuery::from_message(message).unwrap()
        };
        let body = wire_response(&other_query, 0x1234, ResponseCode::NoError);
        assert!(matches!(
            validate_response(
                &connector(),
                &expected_query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(200, Some("application/dns-message"), &body),
                &crate::dns::Cancellation::new(),
            ),
            UpstreamOutcome::TransportFailure(failure)
                if failure.class == TransportFailureClass::QuestionMismatch
        ));
    }

    #[test]
    fn preserves_terminal_dns_response_and_prioritizes_cancellation() {
        let query = query();
        let body = wire_response(&query, 0x1234, ResponseCode::NXDomain);
        assert!(matches!(
            validate_response(
                &connector(),
                &query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(200, Some("application/dns-message"), &body),
                &crate::dns::Cancellation::new(),
            ),
            UpstreamOutcome::Response(response) if response.class() == crate::dns::ResponseClass::NxDomain
        ));

        let cancellation = crate::dns::Cancellation::new();
        cancellation.cancel(CancelReason::DeadlineExceeded);
        assert!(matches!(
            validate_response(
                &connector(),
                &query,
                DnsMessageId::new(0x1234),
                DohHttpResponse::new(500, Some("application/json"), &[0xff]),
                &cancellation,
            ),
            UpstreamOutcome::Cancelled(CancelReason::DeadlineExceeded)
        ));
    }
}
