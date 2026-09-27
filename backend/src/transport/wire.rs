use hickory_proto::op::Message;
use hickory_proto::serialize::binary::{BinEncodable, BinEncoder};
use thiserror::Error;

use crate::dns::{CanonicalMessageError, CanonicalQuery, CanonicalResponse, DnsMessageId};

pub const MAX_DNS_WIRE_BYTES: usize = u16::MAX as usize;

const DNS_HEADER_BYTES: usize = 12;
const DNS_RESPONSE_FLAG: u16 = 0x8000;
// 错误响应只继承 opcode、RD 和 CD，避免复制请求中的响应专属或保留位。
const DNS_SAFE_ERROR_FLAG_MASK: u16 = 0x7910;
const DNS_RCODE_FORMERR: u16 = 1;
const DNS_RCODE_NOTIMP: u16 = 4;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WireError {
    #[error("DNS wire message is empty")]
    Empty,
    #[error("DNS wire message exceeds the {limit} byte limit")]
    TooLarge { limit: usize },
    #[error("DNS wire message could not be decoded")]
    Decode,
    #[error("DNS query failed canonical validation: {0}")]
    InvalidQuery(CanonicalMessageError),
    #[error("DNS response could not be encoded")]
    Encode,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedQuery {
    pub id: DnsMessageId,
    pub query: CanonicalQuery,
}

pub fn decode_query(bytes: &[u8], max_bytes: usize) -> Result<ParsedQuery, WireError> {
    let limit = max_bytes.min(MAX_DNS_WIRE_BYTES);
    if bytes.is_empty() {
        return Err(WireError::Empty);
    }
    if bytes.len() > limit {
        return Err(WireError::TooLarge { limit });
    }
    let message = Message::from_vec(bytes).map_err(|_| WireError::Decode)?;
    let id = DnsMessageId::new(message.metadata.id);
    let query = CanonicalQuery::from_message(message).map_err(WireError::InvalidQuery)?;
    Ok(ParsedQuery { id, query })
}

/// 在无需解析 question 的前提下，为具有可靠 header 的非法查询生成错误响应。
///
/// EDNS BADVERS 需要合法 OPT response，无法仅凭 header 安全构造，因此继续交由调用方丢弃。
pub(super) fn encode_query_error_response(bytes: &[u8], error: &WireError) -> Option<Vec<u8>> {
    if bytes.len() < DNS_HEADER_BYTES {
        return None;
    }
    let request_flags = u16::from_be_bytes([bytes[2], bytes[3]]);
    if request_flags & DNS_RESPONSE_FLAG != 0 {
        return None;
    }
    let response_code = match error {
        WireError::Decode | WireError::InvalidQuery(CanonicalMessageError::QuestionCount(_)) => {
            DNS_RCODE_FORMERR
        }
        WireError::InvalidQuery(CanonicalMessageError::UnsupportedOpCode(_)) => DNS_RCODE_NOTIMP,
        WireError::Empty
        | WireError::TooLarge { .. }
        | WireError::Encode
        | WireError::InvalidQuery(
            CanonicalMessageError::UnexpectedMessageType { .. }
            | CanonicalMessageError::UnsupportedEdnsVersion(_)
            | CanonicalMessageError::MessageIdMismatch { .. }
            | CanonicalMessageError::QuestionMismatch,
        ) => return None,
    };
    let response_flags =
        DNS_RESPONSE_FLAG | (request_flags & DNS_SAFE_ERROR_FLAG_MASK) | response_code;
    let mut response = vec![0_u8; DNS_HEADER_BYTES];
    response[..2].copy_from_slice(&bytes[..2]);
    response[2..4].copy_from_slice(&response_flags.to_be_bytes());
    Some(response)
}

pub fn encode_response(
    response: &CanonicalResponse,
    id: DnsMessageId,
    max_bytes: usize,
) -> Result<Vec<u8>, WireError> {
    let limit = max_bytes.min(MAX_DNS_WIRE_BYTES);
    let message = response_message(response, id);
    let bytes = message.to_vec().map_err(|_| WireError::Encode)?;
    if bytes.len() > limit {
        return Err(WireError::TooLarge { limit });
    }
    Ok(bytes)
}

/// 编码允许在资源记录边界截断的响应；hickory 会同步设置 DNS TC 标志。
pub fn encode_response_truncated(
    response: &CanonicalResponse,
    id: DnsMessageId,
    max_bytes: usize,
) -> Result<Vec<u8>, WireError> {
    let limit = max_bytes.min(MAX_DNS_WIRE_BYTES);
    if limit == 0 {
        return Err(WireError::TooLarge { limit });
    }

    let message = response_message(response, id);
    let mut bytes = Vec::with_capacity(limit.min(512));
    let mut encoder = BinEncoder::new(&mut bytes);
    encoder.set_max_size(limit as u16);
    message.emit(&mut encoder).map_err(|error| match error {
        hickory_proto::ProtoError::MaxBufferSizeExceeded(_) => WireError::TooLarge { limit },
        _ => WireError::Encode,
    })?;
    drop(encoder);

    if bytes.len() > limit {
        return Err(WireError::TooLarge { limit });
    }
    Ok(bytes)
}

fn response_message(response: &CanonicalResponse, id: DnsMessageId) -> Message {
    let mut message = response.as_message().clone();
    message.metadata.id = id.value();
    message
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RData, Record, RecordType, rdata::A};

    use crate::dns::{CanonicalMessageError, CanonicalQuery, CanonicalResponse, DnsMessageId};

    use super::{
        MAX_DNS_WIRE_BYTES, WireError, decode_query, encode_query_error_response, encode_response,
        encode_response_truncated,
    };

    fn query(id: u16) -> Message {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("Example.COM").unwrap(),
            RecordType::A,
        ));
        message
    }

    #[test]
    fn decode_separates_wire_id_and_canonicalizes_question() {
        let parsed = decode_query(&query(0x1234).to_vec().unwrap(), 512).unwrap();

        assert_eq!(parsed.id, DnsMessageId::new(0x1234));
        assert_eq!(parsed.query.as_message().metadata.id, 0);
        assert_eq!(parsed.query.question().name().to_ascii(), "example.com.");
    }

    #[test]
    fn decode_rejects_empty_oversize_and_invalid_wire() {
        assert_eq!(decode_query(&[], 512), Err(WireError::Empty));
        assert_eq!(
            decode_query(&[0_u8; 513], 512),
            Err(WireError::TooLarge { limit: 512 })
        );
        assert_eq!(decode_query(&[0xff, 0x00], 512), Err(WireError::Decode));
    }

    #[test]
    fn decode_caps_caller_limit_at_dns_wire_maximum() {
        let bytes = vec![0_u8; MAX_DNS_WIRE_BYTES + 1];

        assert_eq!(
            decode_query(&bytes, MAX_DNS_WIRE_BYTES + 1),
            Err(WireError::TooLarge {
                limit: MAX_DNS_WIRE_BYTES
            })
        );
    }

    /// 验证非法 query 仅在 header 可靠时生成 FORMERR/NOTIMP，且不回显 question。
    #[test]
    fn query_error_response_preserves_safe_header_fields() {
        let mut malformed = vec![0_u8; 12];
        malformed[..2].copy_from_slice(&0xbeef_u16.to_be_bytes());
        malformed[2..4].copy_from_slice(&0x0110_u16.to_be_bytes());
        let response = encode_query_error_response(
            &malformed,
            &WireError::InvalidQuery(CanonicalMessageError::QuestionCount(0)),
        )
        .unwrap();
        let decoded = Message::from_vec(&response).unwrap();
        assert_eq!(decoded.metadata.id, 0xbeef);
        assert_eq!(decoded.metadata.message_type, MessageType::Response);
        assert_eq!(decoded.metadata.response_code, ResponseCode::FormErr);
        assert!(decoded.metadata.recursion_desired);
        assert!(decoded.metadata.checking_disabled);
        assert!(decoded.queries.is_empty());

        malformed[2..4].copy_from_slice(&0x1100_u16.to_be_bytes());
        let response = encode_query_error_response(
            &malformed,
            &WireError::InvalidQuery(CanonicalMessageError::UnsupportedOpCode(OpCode::Status)),
        )
        .unwrap();
        let decoded = Message::from_vec(&response).unwrap();
        assert_eq!(decoded.metadata.op_code, OpCode::Status);
        assert_eq!(decoded.metadata.response_code, ResponseCode::NotImp);

        assert!(encode_query_error_response(&malformed[..11], &WireError::Decode).is_none());
        malformed[2..4].copy_from_slice(&0x8000_u16.to_be_bytes());
        assert!(encode_query_error_response(&malformed, &WireError::Decode).is_none());
    }

    #[test]
    fn encode_restores_id_without_mutating_canonical_response() {
        let query = CanonicalQuery::from_message(query(7)).unwrap();
        let mut response_message = Message::new(0, MessageType::Response, OpCode::Query);
        response_message.metadata.response_code = ResponseCode::NoError;
        response_message.add_query(query.question().to_query_for_test());
        let response =
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(0))
                .unwrap();

        let bytes = encode_response(&response, DnsMessageId::new(0xbeef), 512).unwrap();
        let encoded = Message::from_vec(&bytes).unwrap();
        assert_eq!(encoded.metadata.id, 0xbeef);
        assert_eq!(response.as_message().metadata.id, 0);
    }

    #[test]
    fn encode_enforces_output_limit() {
        let query = CanonicalQuery::from_message(query(7)).unwrap();
        let mut response_message = Message::new(0, MessageType::Response, OpCode::Query);
        response_message.add_query(query.question().to_query_for_test());
        let response =
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(0))
                .unwrap();

        assert_eq!(
            encode_response(&response, DnsMessageId::new(1), 1),
            Err(WireError::TooLarge { limit: 1 })
        );
    }

    #[test]
    fn truncated_encoding_stops_at_record_boundary_and_sets_tc() {
        let query = CanonicalQuery::from_message(query(7)).unwrap();
        let mut response_message = Message::new(0, MessageType::Response, OpCode::Query);
        response_message.add_query(query.question().to_query_for_test());
        for octet in 1..=40_u8 {
            response_message.add_answer(Record::from_rdata(
                Name::from_str("example.com.").unwrap(),
                60,
                RData::A(A(std::net::Ipv4Addr::new(192, 0, 2, octet))),
            ));
        }
        let response =
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(0))
                .unwrap();

        let bytes = encode_response_truncated(&response, DnsMessageId::new(0xbeef), 512).unwrap();
        let encoded = Message::from_vec(&bytes).unwrap();

        assert!(bytes.len() <= 512);
        assert!(encoded.metadata.truncation);
        assert!(encoded.answers.len() < 40);
        assert_eq!(encoded.metadata.id, 0xbeef);
        assert_eq!(response.as_message().answers.len(), 40);
        assert!(!response.as_message().metadata.truncation);
    }

    trait QuestionForTest {
        fn to_query_for_test(&self) -> Query;
    }

    impl QuestionForTest for crate::dns::CanonicalQuestion {
        fn to_query_for_test(&self) -> Query {
            Query::query(self.name().clone(), self.query_type())
        }
    }
}
