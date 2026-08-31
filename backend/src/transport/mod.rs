//! 入站 transport 的共享协议边界。

pub mod doh;
mod tcp;
mod udp;
mod wire;

pub use doh::{
    DohHttpError, DohHttpMethod, DohHttpStatus, DohRouteError, DohRouteMatch, DohRoutePattern,
    MAX_DOH_GET_DNS_CHARS, MAX_DOH_HEADER_BYTES, MAX_DOH_POST_BODY_BYTES,
    MAX_DOH_REQUEST_TARGET_BYTES, ParsedDohRequest, encode_dns_response, encode_http_error,
    encode_http_error_with_close, encode_http_response, try_parse_request,
};

pub use tcp::{
    TCP_FRAME_PREFIX_BYTES, TcpAdapter, TcpAdapterError, TcpFrameError, TcpSession,
    decode_frame_length, encode_frame,
};
pub use udp::{DEFAULT_REQUEST_TIMEOUT, UdpAdapter, UdpAdapterError};
pub use wire::{MAX_DNS_WIRE_BYTES, ParsedQuery, WireError, decode_query, encode_response};
