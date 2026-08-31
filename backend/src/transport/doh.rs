//! DoH plain HTTP request/response codec.
//!
//! This module deliberately stops at the HTTP envelope.  TLS, PROXY protocol,
//! and listener supervision are wired by later layers; the codec only accepts
//! a bounded HTTP/1.x request and returns a canonical DNS query payload.

use std::fmt;

use thiserror::Error;

use crate::dns::ClientId;

use super::wire::{MAX_DNS_WIRE_BYTES, ParsedQuery, WireError, decode_query};

pub const MAX_DOH_POST_BODY_BYTES: usize = MAX_DNS_WIRE_BYTES;
pub const MAX_DOH_GET_DNS_CHARS: usize = 87_380;
pub const MAX_DOH_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_DOH_REQUEST_TARGET_BYTES: usize = 131_072;

const MAX_HEADER_COUNT: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DohHttpMethod {
    Get,
    Post,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDohRequest {
    pub method: DohHttpMethod,
    pub path: String,
    pub query: ParsedQuery,
    pub wire: Vec<u8>,
    pub connection_close: bool,
    pub consumed_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DohRoutePattern {
    template: String,
    strategy: String,
    placeholder: Option<(usize, usize)>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct DohRouteMatch {
    pub template: String,
    pub strategy: String,
    pub client_id: Option<ClientId>,
}

impl fmt::Debug for DohRouteMatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DohRouteMatch")
            .field("template", &self.template)
            .field("strategy", &self.strategy)
            .field(
                "has_client_id",
                &self
                    .client_id
                    .as_ref()
                    .is_some_and(|id| !id.as_str().is_empty()),
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum DohRouteError {
    #[error("DoH route path must be a non-empty absolute path")]
    InvalidPath,
    #[error("DoH route path contains an invalid client_id placeholder")]
    InvalidPlaceholder,
    #[error("DoH route strategy must not be empty")]
    EmptyStrategy,
}

impl DohRoutePattern {
    pub fn new(
        template: impl Into<String>,
        strategy: impl Into<String>,
    ) -> Result<Self, DohRouteError> {
        let template = template.into();
        let strategy = strategy.into();
        if template.is_empty()
            || !template.starts_with('/')
            || template.contains('?')
            || template.contains('#')
            || template
                .as_bytes()
                .iter()
                .any(|byte| *byte < 0x20 || *byte == 0x7f)
        {
            return Err(DohRouteError::InvalidPath);
        }
        if strategy.trim().is_empty() {
            return Err(DohRouteError::EmptyStrategy);
        }

        let marker = "{client_id}";
        let first = template.find(marker);
        if first.is_some_and(|index| template[index + marker.len()..].contains(marker)) {
            return Err(DohRouteError::InvalidPlaceholder);
        }
        let placeholder = first.map(|start| (start, start + marker.len()));
        if let Some((start, end)) = placeholder {
            let is_segment_start = start == 0 || template.as_bytes()[start - 1] == b'/';
            let is_segment_end = end == template.len() || template.as_bytes()[end] == b'/';
            if !is_segment_start || !is_segment_end {
                return Err(DohRouteError::InvalidPlaceholder);
            }
        }

        Ok(Self {
            template,
            strategy,
            placeholder,
        })
    }

    pub fn template(&self) -> &str {
        &self.template
    }

    pub fn strategy(&self) -> &str {
        &self.strategy
    }

    pub fn matches(&self, path: &str) -> Option<DohRouteMatch> {
        let client_id = match self.placeholder {
            None if path == self.template => None,
            Some((start, end)) => {
                if !path.starts_with(&self.template[..start])
                    || !path.ends_with(&self.template[end..])
                {
                    return None;
                }
                let value_end = path.len().checked_sub(self.template.len() - end)?;
                let value = &path[start..value_end];
                if value.is_empty() || value.contains('/') || value.contains('?') {
                    return None;
                }
                Some(ClientId::new(value.to_owned()))
            }
            _ => return None,
        };
        Some(DohRouteMatch {
            template: self.template.clone(),
            strategy: self.strategy.clone(),
            client_id,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DohHttpStatus {
    Ok,
    BadRequest,
    NotFound,
    MethodNotAllowed,
    PayloadTooLarge,
    UriTooLong,
    UnsupportedMediaType,
    InternalServerError,
}

impl DohHttpStatus {
    pub const fn code(self) -> u16 {
        match self {
            Self::Ok => 200,
            Self::BadRequest => 400,
            Self::NotFound => 404,
            Self::MethodNotAllowed => 405,
            Self::PayloadTooLarge => 413,
            Self::UriTooLong => 414,
            Self::UnsupportedMediaType => 415,
            Self::InternalServerError => 500,
        }
    }

    const fn reason(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::BadRequest => "Bad Request",
            Self::NotFound => "Not Found",
            Self::MethodNotAllowed => "Method Not Allowed",
            Self::PayloadTooLarge => "Payload Too Large",
            Self::UriTooLong => "URI Too Long",
            Self::UnsupportedMediaType => "Unsupported Media Type",
            Self::InternalServerError => "Internal Server Error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum DohHttpError {
    #[error("HTTP request is incomplete")]
    Incomplete,
    #[error("HTTP request is malformed")]
    Malformed,
    #[error("HTTP method is not supported")]
    MethodNotAllowed,
    #[error("DoH route was not found")]
    NotFound,
    #[error("GET request is missing the dns parameter")]
    MissingDnsParameter,
    #[error("GET request contains duplicate dns parameters")]
    DuplicateDnsParameter,
    #[error("GET dns parameter is not valid unpadded base64url")]
    InvalidDnsParameter,
    #[error("DoH request target is too long")]
    UriTooLong,
    #[error("DoH POST body is too large")]
    PayloadTooLarge,
    #[error("DoH POST content type is unsupported")]
    UnsupportedMediaType,
    #[error("DNS wire message is invalid")]
    InvalidDnsWire,
    #[error("transfer encoding is unsupported")]
    UnsupportedTransferEncoding,
    #[error("POST request requires a content length")]
    MissingContentLength,
}

impl DohHttpError {
    pub const fn status(self) -> DohHttpStatus {
        match self {
            Self::UriTooLong => DohHttpStatus::UriTooLong,
            Self::PayloadTooLarge => DohHttpStatus::PayloadTooLarge,
            Self::UnsupportedMediaType => DohHttpStatus::UnsupportedMediaType,
            Self::MethodNotAllowed => DohHttpStatus::MethodNotAllowed,
            Self::NotFound => DohHttpStatus::NotFound,
            Self::Incomplete
            | Self::Malformed
            | Self::MissingDnsParameter
            | Self::DuplicateDnsParameter
            | Self::InvalidDnsParameter
            | Self::InvalidDnsWire
            | Self::UnsupportedTransferEncoding
            | Self::MissingContentLength => DohHttpStatus::BadRequest,
        }
    }

    pub const fn should_close(self) -> bool {
        matches!(
            self,
            Self::Incomplete
                | Self::Malformed
                | Self::UriTooLong
                | Self::PayloadTooLarge
                | Self::UnsupportedTransferEncoding
        )
    }
}

/// Parse one complete HTTP request from the front of `buffer`.
///
/// `Ok(None)` means more bytes are needed.  Any returned error is terminal for
/// the current connection; callers should write the corresponding HTTP status
/// and close when `should_close()` is true.
pub fn try_parse_request(buffer: &[u8]) -> Result<Option<ParsedDohRequest>, DohHttpError> {
    let Some(header_end) = find_subslice(buffer, b"\r\n\r\n") else {
        if buffer.len() > MAX_DOH_HEADER_BYTES {
            return Err(DohHttpError::Malformed);
        }
        return Ok(None);
    };
    if header_end > MAX_DOH_HEADER_BYTES {
        return Err(DohHttpError::Malformed);
    }

    let header = &buffer[..header_end];
    if header.iter().any(|byte| *byte >= 0x80) {
        return Err(DohHttpError::Malformed);
    }
    let header = std::str::from_utf8(header).map_err(|_| DohHttpError::Malformed)?;
    let mut lines = header.split("\r\n");
    let request_line = lines.next().ok_or(DohHttpError::Malformed)?;
    let parts = request_line.split(' ').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(DohHttpError::Malformed);
    }
    let method = match parts[0] {
        "GET" => DohHttpMethod::Get,
        "POST" => DohHttpMethod::Post,
        _ => return Err(DohHttpError::MethodNotAllowed),
    };
    if parts[2] != "HTTP/1.1" && parts[2] != "HTTP/1.0" {
        return Err(DohHttpError::Malformed);
    }
    if parts[1].len() > MAX_DOH_REQUEST_TARGET_BYTES {
        return Err(DohHttpError::UriTooLong);
    }
    let target = parts[1];

    let mut content_length = None;
    let mut content_type = None;
    let mut connection_close = parts[2] == "HTTP/1.0";
    let mut header_count = 0_usize;
    for line in lines {
        if line.is_empty() {
            return Err(DohHttpError::Malformed);
        }
        header_count += 1;
        if header_count > MAX_HEADER_COUNT {
            return Err(DohHttpError::Malformed);
        }
        let separator = line.find(':').ok_or(DohHttpError::Malformed)?;
        let (name, value) = line.split_at(separator);
        let value = &value[1..];
        if name.is_empty() || !name.as_bytes().iter().copied().all(is_token_byte) {
            return Err(DohHttpError::Malformed);
        }
        if value
            .as_bytes()
            .iter()
            .any(|byte| *byte < 0x20 && *byte != b'\t' || *byte == 0x7f)
        {
            return Err(DohHttpError::Malformed);
        }
        let name = name.to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                if content_length.is_some() {
                    return Err(DohHttpError::Malformed);
                }
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| DohHttpError::Malformed)?;
                content_length = Some(parsed);
            }
            "content-type" => {
                if content_type.replace(value.to_owned()).is_some() {
                    return Err(DohHttpError::Malformed);
                }
            }
            "transfer-encoding" => return Err(DohHttpError::UnsupportedTransferEncoding),
            "connection"
                if value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("close")) =>
            {
                connection_close = true;
            }
            _ => {}
        }
    }

    let body_length = content_length.unwrap_or(0);
    if body_length > MAX_DOH_POST_BODY_BYTES {
        return Err(DohHttpError::PayloadTooLarge);
    }
    let body_start = header_end + 4;
    let body_end = body_start
        .checked_add(body_length)
        .ok_or(DohHttpError::PayloadTooLarge)?;
    if buffer.len() < body_end {
        return Ok(None);
    }
    let body = &buffer[body_start..body_end];
    let (path, query_string) = split_target(target)?;

    let wire = match method {
        DohHttpMethod::Get => {
            if body_length != 0 {
                return Err(DohHttpError::Malformed);
            }
            let dns_value = get_dns_parameter(query_string)?;
            if dns_value.len() > MAX_DOH_GET_DNS_CHARS {
                return Err(DohHttpError::UriTooLong);
            }
            let dns_value =
                std::str::from_utf8(&dns_value).map_err(|_| DohHttpError::InvalidDnsParameter)?;
            decode_base64url(dns_value).map_err(|_| DohHttpError::InvalidDnsParameter)?
        }
        DohHttpMethod::Post => {
            if content_length.is_none() {
                return Err(DohHttpError::MissingContentLength);
            }
            if content_type.as_deref() != Some("application/dns-message") {
                return Err(DohHttpError::UnsupportedMediaType);
            }
            if body.is_empty() {
                return Err(DohHttpError::InvalidDnsWire);
            }
            body.to_vec()
        }
    };
    let query = decode_query(&wire, MAX_DNS_WIRE_BYTES).map_err(|error| match error {
        WireError::TooLarge { .. } => DohHttpError::PayloadTooLarge,
        _ => DohHttpError::InvalidDnsWire,
    })?;

    Ok(Some(ParsedDohRequest {
        method,
        path: path.to_owned(),
        query,
        wire,
        connection_close,
        consumed_bytes: body_end,
    }))
}

pub fn encode_http_response(
    status: DohHttpStatus,
    body: &[u8],
    content_type: Option<&str>,
    close: bool,
) -> Vec<u8> {
    encode_http_response_with_allow(status, body, content_type, close, None)
}

pub fn encode_http_error(error: DohHttpError) -> Vec<u8> {
    let allow = (error == DohHttpError::MethodNotAllowed).then_some("GET, POST");
    encode_http_response_with_allow(error.status(), &[], None, error.should_close(), allow)
}

pub fn encode_dns_response(body: &[u8], close: bool) -> Vec<u8> {
    encode_http_response_with_allow(
        DohHttpStatus::Ok,
        body,
        Some("application/dns-message"),
        close,
        None,
    )
}

fn encode_http_response_with_allow(
    status: DohHttpStatus,
    body: &[u8],
    content_type: Option<&str>,
    close: bool,
    allow: Option<&str>,
) -> Vec<u8> {
    let mut response = format!("HTTP/1.1 {} {}\r\n", status.code(), status.reason());
    if let Some(allow) = allow {
        response.push_str("Allow: ");
        response.push_str(allow);
        response.push_str("\r\n");
    }
    if let Some(content_type) = content_type {
        response.push_str("Content-Type: ");
        response.push_str(content_type);
        response.push_str("\r\n");
    }
    response.push_str("Cache-Control: no-store\r\n");
    response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    if close {
        response.push_str("Connection: close\r\n");
    }
    response.push_str("\r\n");
    let mut bytes = response.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn split_target(target: &str) -> Result<(&str, &str), DohHttpError> {
    if !target.starts_with('/') || target.contains('#') {
        return Err(DohHttpError::Malformed);
    }
    match target.split_once('?') {
        Some((path, query)) if !path.is_empty() => Ok((path, query)),
        Some(_) => Err(DohHttpError::Malformed),
        None => Ok((target, "")),
    }
}

fn get_dns_parameter(query: &str) -> Result<Vec<u8>, DohHttpError> {
    let mut result = None;
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or(DohHttpError::Malformed)?;
        let key = percent_decode(key).map_err(|_| DohHttpError::Malformed)?;
        if key != b"dns" {
            continue;
        }
        if result.is_some() {
            return Err(DohHttpError::DuplicateDnsParameter);
        }
        result = Some(percent_decode(value).map_err(|_| DohHttpError::InvalidDnsParameter)?);
    }
    result.ok_or(DohHttpError::MissingDnsParameter)
}

fn percent_decode(value: &str) -> Result<Vec<u8>, ()> {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(());
            }
            let high = hex_value(bytes[index + 1]).ok_or(())?;
            let low = hex_value(bytes[index + 2]).ok_or(())?;
            output.push((high << 4) | low);
            index += 3;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    Ok(output)
}

fn decode_base64url(value: &str) -> Result<Vec<u8>, ()> {
    if value.is_empty() || value.contains('=') || value.len() % 4 == 1 {
        return Err(());
    }
    let mut output = Vec::with_capacity(value.len() * 3 / 4);
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in value.bytes() {
        let sextet = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return Err(()),
        } as u32;
        accumulator = (accumulator << 6) | sextet;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((accumulator >> bits) as u8);
            accumulator &= (1_u32 << bits).saturating_sub(1);
            if output.len() > MAX_DNS_WIRE_BYTES {
                return Err(());
            }
        }
    }
    if bits > 0 && accumulator != 0 {
        return Err(());
    }
    Ok(output)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use hickory_proto::op::{Message, MessageType, OpCode, Query};
    use hickory_proto::rr::{Name, RecordType};

    use super::*;

    fn wire() -> Vec<u8> {
        let mut message = Message::new(0x1234, MessageType::Query, OpCode::Query);
        message.add_query(Query::query(
            Name::from_str("Example.COM.").unwrap(),
            RecordType::A,
        ));
        message.to_vec().unwrap()
    }

    fn base64url(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut output = String::new();
        let mut index = 0;
        while index < bytes.len() {
            let first = bytes[index];
            let second = bytes.get(index + 1).copied();
            let third = bytes.get(index + 2).copied();
            output.push(TABLE[(first >> 2) as usize] as char);
            output.push(TABLE[((first & 0x03) << 4 | second.unwrap_or(0) >> 4) as usize] as char);
            if let Some(second) = second {
                output
                    .push(TABLE[((second & 0x0f) << 2 | third.unwrap_or(0) >> 6) as usize] as char);
            }
            if let Some(third) = third {
                output.push(TABLE[(third & 0x3f) as usize] as char);
            }
            index += 3;
        }
        output
    }

    fn request(method: &str, target: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut bytes = format!("{method} {target} HTTP/1.1\r\n{headers}\r\n").into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn parses_get_and_restores_wire_id_metadata() {
        let wire = wire();
        let encoded = base64url(&wire);
        let request_bytes = request(
            "GET",
            &format!("/dns/{}/?dns={encoded}", "client"),
            "Host: example\r\n",
            &[],
        );

        let parsed = try_parse_request(&request_bytes).unwrap().unwrap();
        assert_eq!(parsed.method, DohHttpMethod::Get);
        assert_eq!(parsed.path, "/dns/client/");
        assert_eq!(parsed.query.id.value(), 0x1234);
        assert_eq!(parsed.query.query.as_message().metadata.id, 0);
        assert_eq!(parsed.consumed_bytes, request_bytes.len());
    }

    #[test]
    fn parses_post_and_requires_exact_media_type() {
        let wire = wire();
        let request_bytes = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: application/dns-message\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        assert_eq!(
            try_parse_request(&request_bytes).unwrap().unwrap().wire,
            wire
        );

        let invalid = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: application/octet-stream\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        assert_eq!(
            try_parse_request(&invalid),
            Err(DohHttpError::UnsupportedMediaType)
        );
    }

    #[test]
    fn maps_http_protocol_boundaries_to_stable_statuses() {
        assert_eq!(DohHttpError::MethodNotAllowed.status().code(), 405);
        assert_eq!(DohHttpError::UnsupportedMediaType.status().code(), 415);
        assert_eq!(DohHttpError::PayloadTooLarge.status().code(), 413);
        assert_eq!(DohHttpError::UriTooLong.status().code(), 414);
        let response = encode_http_error(DohHttpError::MethodNotAllowed);
        let text = String::from_utf8(response).unwrap();
        assert!(text.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"));
        assert!(text.contains("Allow: GET, POST\r\n"));
        assert!(text.contains("Content-Length: 0\r\n"));
    }

    #[test]
    fn rejects_missing_duplicate_and_padded_get_parameters() {
        let wire = wire();
        let encoded = base64url(&wire);
        let missing = request("GET", "/dns", "", &[]);
        assert_eq!(
            try_parse_request(&missing),
            Err(DohHttpError::MissingDnsParameter)
        );
        let duplicate = request("GET", &format!("/dns?dns={encoded}&dns={encoded}"), "", &[]);
        assert_eq!(
            try_parse_request(&duplicate),
            Err(DohHttpError::DuplicateDnsParameter)
        );
        let padded = request("GET", &format!("/dns?dns={encoded}="), "", &[]);
        assert_eq!(
            try_parse_request(&padded),
            Err(DohHttpError::InvalidDnsParameter)
        );
        let percent_encoded = request(
            "GET",
            &format!("/dns?dns={}", encoded.replace('-', "%2D")),
            "",
            &[],
        );
        assert_eq!(
            try_parse_request(&percent_encoded)
                .unwrap()
                .unwrap()
                .query
                .id
                .value(),
            0x1234
        );
    }

    #[test]
    fn returns_incomplete_until_headers_and_body_are_available() {
        let wire = wire();
        let full = request(
            "POST",
            "/dns",
            &format!(
                "Content-Type: application/dns-message\r\nContent-Length: {}\r\n",
                wire.len()
            ),
            &wire,
        );
        let header_end = find_subslice(&full, b"\r\n\r\n").unwrap() + 4;
        assert_eq!(try_parse_request(&full[..header_end - 1]).unwrap(), None);
        assert_eq!(
            try_parse_request(&full[..header_end + wire.len() - 1]).unwrap(),
            None
        );
    }

    #[test]
    fn route_pattern_matches_exact_and_client_id_segments() {
        let exact = DohRoutePattern::new("/dns", "default").unwrap();
        assert!(exact.matches("/dns").is_some());
        assert!(exact.matches("/dns/extra").is_none());

        let templated = DohRoutePattern::new("/dns/{client_id}", "inner").unwrap();
        let matched = templated.matches("/dns/abc-123").unwrap();
        assert_eq!(matched.strategy, "inner");
        assert_eq!(matched.client_id.unwrap().as_str(), "abc-123");
        assert!(templated.matches("/dns/a/b").is_none());
    }

    #[test]
    fn route_pattern_rejects_embedded_or_repeated_placeholder() {
        assert_eq!(
            DohRoutePattern::new("/dns/{client_id}/x/{client_id}", "default"),
            Err(DohRouteError::InvalidPlaceholder)
        );
        assert_eq!(
            DohRoutePattern::new("/dns/pre{client_id}", "default"),
            Err(DohRouteError::InvalidPlaceholder)
        );
    }
}
