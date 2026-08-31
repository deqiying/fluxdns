//! 入站 transport 的共享协议边界。

mod udp;
mod wire;

pub use udp::{DEFAULT_REQUEST_TIMEOUT, UdpAdapter, UdpAdapterError};
pub use wire::{MAX_DNS_WIRE_BYTES, ParsedQuery, WireError, decode_query, encode_response};
