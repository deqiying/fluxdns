use hickory_proto::op::Message;
use thiserror::Error;

use crate::dns::{CanonicalMessageError, CanonicalQuery, CanonicalResponse, DnsMessageId};

pub const MAX_DNS_WIRE_BYTES: usize = u16::MAX as usize;

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

pub fn encode_response(
    response: &CanonicalResponse,
    id: DnsMessageId,
    max_bytes: usize,
) -> Result<Vec<u8>, WireError> {
    let limit = max_bytes.min(MAX_DNS_WIRE_BYTES);
    let mut message = response.as_message().clone();
    message.metadata.id = id.value();
    let bytes = message.to_vec().map_err(|_| WireError::Encode)?;
    if bytes.len() > limit {
        return Err(WireError::TooLarge { limit });
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
    use hickory_proto::rr::{Name, RecordType};

    use crate::dns::{CanonicalQuery, CanonicalResponse, DnsMessageId};

    use super::{MAX_DNS_WIRE_BYTES, WireError, decode_query, encode_response};

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

    trait QuestionForTest {
        fn to_query_for_test(&self) -> Query;
    }

    impl QuestionForTest for crate::dns::CanonicalQuestion {
        fn to_query_for_test(&self) -> Query {
            Query::query(self.name().clone(), self.query_type())
        }
    }
}
