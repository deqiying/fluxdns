use std::fmt;

use hickory_proto::{
    op::{Message, MessageType, OpCode, Query, ResponseCode},
    rr::{DNSClass, Name, RData, RecordType},
};
use thiserror::Error;

/// transport/upstream 为一条 DNS wire message 分配的关联 ID。
///
/// canonical message 内部始终使用 ID 0；adapter 在编码请求时显式设置该值，
/// 并在接受响应前把同一个值交回校验器。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DnsMessageId(u16);

impl DnsMessageId {
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u16 {
        self.0
    }
}

/// 经过大小写和根点规范化的 DNS question。
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct CanonicalQuestion {
    name: Name,
    query_type: RecordType,
    query_class: DNSClass,
}

impl fmt::Debug for CanonicalQuestion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CanonicalQuestion")
            .field("label_count", &self.name.num_labels())
            .field("query_type", &self.query_type)
            .field("query_class", &self.query_class)
            .finish()
    }
}

impl CanonicalQuestion {
    fn from_query(query: &Query) -> Self {
        let mut name = query.name().to_lowercase();
        name.set_fqdn(true);
        Self {
            name,
            query_type: query.query_type(),
            query_class: query.query_class(),
        }
    }

    fn to_query(&self) -> Query {
        let mut query = Query::query(self.name.clone(), self.query_type);
        query.set_query_class(self.query_class);
        query
    }

    pub fn name(&self) -> &Name {
        &self.name
    }

    pub fn query_type(&self) -> RecordType {
        self.query_type
    }

    pub fn query_class(&self) -> DNSClass {
        self.query_class
    }
}

/// 与 transport envelope 和客户端 DNS ID 解耦的 query。
#[derive(Clone, Eq, PartialEq)]
pub struct CanonicalQuery {
    message: Message,
    question: CanonicalQuestion,
}

impl fmt::Debug for CanonicalQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let edns = self.message.edns.as_ref();
        formatter
            .debug_struct("CanonicalQuery")
            .field("question", &self.question)
            .field(
                "recursion_desired",
                &self.message.metadata.recursion_desired,
            )
            .field("authentic_data", &self.message.metadata.authentic_data)
            .field(
                "checking_disabled",
                &self.message.metadata.checking_disabled,
            )
            .field("edns_present", &edns.is_some())
            .field(
                "dnssec_ok",
                &edns.is_some_and(|edns| edns.flags().dnssec_ok),
            )
            .finish()
    }
}

impl CanonicalQuery {
    pub fn from_message(message: Message) -> Result<Self, CanonicalMessageError> {
        validate_common(&message, MessageType::Query)?;

        let question = CanonicalQuestion::from_query(&message.queries[0]);
        let mut canonical = Message::new(0, MessageType::Query, OpCode::Query);
        canonical.metadata.recursion_desired = message.metadata.recursion_desired;
        canonical.metadata.authentic_data = message.metadata.authentic_data;
        canonical.metadata.checking_disabled = message.metadata.checking_disabled;
        canonical.add_query(question.to_query());
        if let Some(edns) = message.edns.clone() {
            canonical.set_edns(edns);
        }

        Ok(Self {
            message: canonical,
            question,
        })
    }

    pub fn question(&self) -> &CanonicalQuestion {
        &self.question
    }

    pub fn as_message(&self) -> &Message {
        &self.message
    }

    /// 为 transport/upstream 编码创建带关联 ID 的副本，不修改 canonical query。
    pub fn message_with_id(&self, id: DnsMessageId) -> Message {
        let mut message = self.message.clone();
        message.metadata.id = id.value();
        message
    }

    pub fn into_message(self) -> Message {
        self.message
    }
}

impl TryFrom<Message> for CanonicalQuery {
    type Error = CanonicalMessageError;

    fn try_from(message: Message) -> Result<Self, Self::Error> {
        Self::from_message(message)
    }
}

/// DNS response 的稳定行为分类。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResponseClass {
    Positive,
    NoData,
    NxDomain,
    Refused,
    ServFail,
    Truncated,
    Other(ResponseCode),
}

/// 从 response RR 中提取的原始 TTL 信息。
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct TtlMetadata {
    pub min_ttl: Option<u32>,
    pub negative_ttl: Option<u32>,
}

/// 已完成协议校验且不含上游 DNS ID 的 response。
#[derive(Clone, Eq, PartialEq)]
pub struct CanonicalResponse {
    message: Message,
    class: ResponseClass,
    ttl: TtlMetadata,
}

impl fmt::Debug for CanonicalResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let question = CanonicalQuestion::from_query(&self.message.queries[0]);
        let edns = self.message.edns.as_ref();
        formatter
            .debug_struct("CanonicalResponse")
            .field("question", &question)
            .field("class", &self.class)
            .field("answer_count", &self.message.answers.len())
            .field("authority_count", &self.message.authorities.len())
            .field("additional_count", &self.message.additionals.len())
            .field("authoritative", &self.message.metadata.authoritative)
            .field("truncated", &self.message.metadata.truncation)
            .field(
                "recursion_available",
                &self.message.metadata.recursion_available,
            )
            .field("authentic_data", &self.message.metadata.authentic_data)
            .field(
                "checking_disabled",
                &self.message.metadata.checking_disabled,
            )
            .field("edns_present", &edns.is_some())
            .field(
                "dnssec_ok",
                &edns.is_some_and(|edns| edns.flags().dnssec_ok),
            )
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl CanonicalResponse {
    /// 构造不携带 transport envelope 的空错误响应。
    pub(crate) fn empty_response(
        query: &CanonicalQuery,
        code: ResponseCode,
    ) -> Result<Self, CanonicalMessageError> {
        let mut message = Message::response(0, OpCode::Query);
        message.metadata.recursion_desired = query.as_message().metadata.recursion_desired;
        message.metadata.checking_disabled = query.as_message().metadata.checking_disabled;
        message.metadata.response_code = code;
        message.add_query(query.question().to_query());
        Self::from_message(message, query, DnsMessageId::new(0))
    }

    pub fn from_message(
        mut message: Message,
        expected_query: &CanonicalQuery,
        expected_id: DnsMessageId,
    ) -> Result<Self, CanonicalMessageError> {
        validate_common(&message, MessageType::Response)?;

        if message.metadata.id != expected_id.value() {
            return Err(CanonicalMessageError::MessageIdMismatch {
                expected: expected_id.value(),
                actual: message.metadata.id,
            });
        }

        let actual_question = CanonicalQuestion::from_query(&message.queries[0]);
        if &actual_question != expected_query.question() {
            return Err(CanonicalMessageError::QuestionMismatch);
        }

        message.metadata.id = 0;
        message.queries[0] = actual_question.to_query();
        let class = classify_response(&message);
        let ttl = extract_ttl_metadata(&message, class);

        Ok(Self {
            message,
            class,
            ttl,
        })
    }

    pub fn as_message(&self) -> &Message {
        &self.message
    }

    pub fn into_message(self) -> Message {
        self.message
    }

    pub fn class(&self) -> ResponseClass {
        self.class
    }

    pub fn ttl(&self) -> TtlMetadata {
        self.ttl
    }

    pub fn matches_query(&self, query: &CanonicalQuery) -> bool {
        CanonicalQuestion::from_query(&self.message.queries[0]) == *query.question()
    }
}

/// canonical message 构造失败的稳定分类。
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum CanonicalMessageError {
    #[error("expected DNS {expected:?}, got {actual:?}")]
    UnexpectedMessageType {
        expected: MessageType,
        actual: MessageType,
    },
    #[error("unsupported DNS opcode: {0:?}")]
    UnsupportedOpCode(OpCode),
    #[error("expected exactly one DNS question, got {0}")]
    QuestionCount(usize),
    #[error("unsupported EDNS version: {0}")]
    UnsupportedEdnsVersion(u8),
    #[error("DNS response ID mismatch: expected {expected}, got {actual}")]
    MessageIdMismatch { expected: u16, actual: u16 },
    #[error("DNS response question does not match the expected query")]
    QuestionMismatch,
}

fn validate_common(
    message: &Message,
    expected_type: MessageType,
) -> Result<(), CanonicalMessageError> {
    if message.metadata.message_type != expected_type {
        return Err(CanonicalMessageError::UnexpectedMessageType {
            expected: expected_type,
            actual: message.metadata.message_type,
        });
    }
    if message.metadata.op_code != OpCode::Query {
        return Err(CanonicalMessageError::UnsupportedOpCode(
            message.metadata.op_code,
        ));
    }
    if message.queries.len() != 1 {
        return Err(CanonicalMessageError::QuestionCount(message.queries.len()));
    }
    if let Some(edns) = &message.edns
        && edns.version() != 0
    {
        return Err(CanonicalMessageError::UnsupportedEdnsVersion(
            edns.version(),
        ));
    }
    Ok(())
}

fn classify_response(message: &Message) -> ResponseClass {
    if message.metadata.truncation {
        return ResponseClass::Truncated;
    }

    match message.metadata.response_code {
        ResponseCode::NoError if message.answers.is_empty() => ResponseClass::NoData,
        ResponseCode::NoError => ResponseClass::Positive,
        ResponseCode::NXDomain => ResponseClass::NxDomain,
        ResponseCode::Refused => ResponseClass::Refused,
        ResponseCode::ServFail => ResponseClass::ServFail,
        code => ResponseClass::Other(code),
    }
}

fn extract_ttl_metadata(message: &Message, class: ResponseClass) -> TtlMetadata {
    let min_ttl = message
        .answers
        .iter()
        .chain(&message.authorities)
        .chain(&message.additionals)
        .map(|record| record.ttl)
        .min();

    let negative_ttl = matches!(class, ResponseClass::NoData | ResponseClass::NxDomain)
        .then(|| {
            message.authorities.iter().filter_map(|record| {
                let RData::SOA(soa) = &record.data else {
                    return None;
                };
                Some(record.ttl.min(soa.minimum))
            })
        })
        .and_then(Iterator::min);

    TtlMetadata {
        min_ttl,
        negative_ttl,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::{
        op::Edns,
        rr::{
            RData, Record,
            rdata::{A, opt::EdnsOption},
        },
    };
    use std::{net::Ipv4Addr, str::FromStr};

    fn query(id: u16, name: &str) -> Message {
        let mut message = Message::new(id, MessageType::Query, OpCode::Query);
        message.metadata.recursion_desired = true;
        message.add_query(Query::query(Name::from_str(name).unwrap(), RecordType::A));
        message
    }

    fn response(expected: &CanonicalQuery, code: ResponseCode) -> Message {
        let mut message = Message::new(77, MessageType::Response, OpCode::Query);
        message.metadata.response_code = code;
        message.add_query(expected.question().to_query());
        message
    }

    #[test]
    fn different_dns_ids_produce_equal_canonical_queries() {
        let first = CanonicalQuery::from_message(query(1, "Example.COM")).unwrap();
        let second = CanonicalQuery::from_message(query(65535, "Example.COM")).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.as_message().metadata.id, 0);
    }

    #[test]
    fn qname_is_lowercase_and_fully_qualified() {
        let query = CanonicalQuery::from_message(query(1, "WWW.Example.COM")).unwrap();

        assert_eq!(query.question().name().to_ascii(), "www.example.com.");
        assert!(query.question().name().is_fqdn());
    }

    #[test]
    fn rejects_wrong_opcode_question_count_and_edns_version() {
        let mut wrong_opcode = query(1, "example.com.");
        wrong_opcode.metadata.op_code = OpCode::Status;
        assert!(matches!(
            CanonicalQuery::from_message(wrong_opcode),
            Err(CanonicalMessageError::UnsupportedOpCode(OpCode::Status))
        ));

        let no_question = Message::new(1, MessageType::Query, OpCode::Query);
        assert_eq!(
            CanonicalQuery::from_message(no_question),
            Err(CanonicalMessageError::QuestionCount(0))
        );

        let mut unsupported_edns = query(1, "example.com.");
        let mut edns = Edns::new();
        edns.set_version(1);
        unsupported_edns.set_edns(edns);
        assert_eq!(
            CanonicalQuery::from_message(unsupported_edns),
            Err(CanonicalMessageError::UnsupportedEdnsVersion(1))
        );
    }

    #[test]
    fn response_requires_matching_question_and_is_classified() {
        let expected = CanonicalQuery::from_message(query(1, "example.com.")).unwrap();
        let mut mismatch = response(&expected, ResponseCode::NoError);
        mismatch.queries[0].set_name(Name::from_str("other.example.").unwrap());
        assert_eq!(
            CanonicalResponse::from_message(mismatch, &expected, DnsMessageId::new(77)),
            Err(CanonicalMessageError::QuestionMismatch)
        );

        let nxdomain = CanonicalResponse::from_message(
            response(&expected, ResponseCode::NXDomain),
            &expected,
            DnsMessageId::new(77),
        )
        .unwrap();
        assert_eq!(nxdomain.class(), ResponseClass::NxDomain);
        assert_eq!(nxdomain.as_message().metadata.id, 0);

        let mut positive = response(&expected, ResponseCode::NoError);
        positive.add_answer(Record::from_rdata(
            Name::from_str("example.com.").unwrap(),
            120,
            RData::A(A(Ipv4Addr::new(192, 0, 2, 1))),
        ));
        let positive =
            CanonicalResponse::from_message(positive, &expected, DnsMessageId::new(77)).unwrap();
        assert_eq!(positive.class(), ResponseClass::Positive);
        assert_eq!(positive.ttl().min_ttl, Some(120));

        let mut truncated = response(&expected, ResponseCode::NoError);
        truncated.metadata.truncation = true;
        let truncated =
            CanonicalResponse::from_message(truncated, &expected, DnsMessageId::new(77)).unwrap();
        assert_eq!(truncated.class(), ResponseClass::Truncated);
    }

    #[test]
    fn response_requires_matching_dns_id_before_canonicalization() {
        let expected = CanonicalQuery::from_message(query(1, "example.com.")).unwrap();
        let response = response(&expected, ResponseCode::NoError);

        assert_eq!(
            CanonicalResponse::from_message(response, &expected, DnsMessageId::new(76)),
            Err(CanonicalMessageError::MessageIdMismatch {
                expected: 76,
                actual: 77,
            })
        );
        assert_eq!(
            expected.message_with_id(DnsMessageId::new(42)).metadata.id,
            42
        );
        assert_eq!(expected.as_message().metadata.id, 0);
    }

    #[test]
    fn empty_response_preserves_question_and_safe_flags() {
        let mut request = query(42, "Example.COM");
        request.metadata.checking_disabled = true;
        let expected = CanonicalQuery::from_message(request).unwrap();

        let response =
            CanonicalResponse::empty_response(&expected, ResponseCode::ServFail).unwrap();

        assert_eq!(response.as_message().metadata.id, 0);
        assert_eq!(
            response.as_message().metadata.message_type,
            MessageType::Response
        );
        assert!(response.as_message().metadata.recursion_desired);
        assert!(response.as_message().metadata.checking_disabled);
        assert!(!response.as_message().metadata.recursion_available);
        assert_eq!(response.as_message().queries, expected.as_message().queries);
        assert!(response.as_message().edns.is_none());
    }

    #[test]
    fn empty_response_is_classified_without_echoing_query_edns() {
        let mut request = query(42, "example.com.");
        let mut edns = Edns::new();
        edns.options_mut()
            .insert(EdnsOption::Subnet("198.51.100.0/24".parse().unwrap()));
        request.set_edns(edns);
        let expected = CanonicalQuery::from_message(request).unwrap();

        let response =
            CanonicalResponse::empty_response(&expected, ResponseCode::ServFail).unwrap();

        assert_eq!(response.class(), ResponseClass::ServFail);
        assert!(response.as_message().edns.is_none());
    }

    #[test]
    fn canonical_debug_redacts_dns_content() {
        const QNAME: &str = "private-service.secret-example.test.";
        const ECS_ADDRESS: &str = "198.51.100.0";
        const RESPONSE_ADDRESS: &str = "203.0.113.99";

        let mut query_message = query(42, QNAME);
        let mut edns = Edns::new();
        edns.options_mut()
            .insert(EdnsOption::Subnet("198.51.100.0/24".parse().unwrap()));
        query_message.set_edns(edns.clone());
        let query = CanonicalQuery::from_message(query_message).unwrap();

        let mut response_message = response(&query, ResponseCode::NoError);
        response_message.set_edns(edns);
        response_message.add_answer(Record::from_rdata(
            Name::from_str(QNAME).unwrap(),
            300,
            RData::A(A(Ipv4Addr::new(203, 0, 113, 99))),
        ));
        let response =
            CanonicalResponse::from_message(response_message, &query, DnsMessageId::new(77))
                .unwrap();

        let question_debug = format!("{:?}", query.question());
        let query_debug = format!("{query:?}");
        let response_debug = format!("{response:?}");
        for debug in [&question_debug, &query_debug, &response_debug] {
            assert!(!debug.contains(QNAME));
            assert!(!debug.contains(ECS_ADDRESS));
            assert!(!debug.contains(RESPONSE_ADDRESS));
            assert!(!debug.contains("Subnet"));
        }

        assert!(query_debug.contains("query_type: A"));
        assert!(response_debug.contains("class: Positive"));
        assert!(response_debug.contains("answer_count: 1"));
        assert!(response_debug.contains("min_ttl: Some(300)"));
    }
}
