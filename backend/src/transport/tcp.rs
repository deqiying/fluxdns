//! TCP DNS framing 边界。

use thiserror::Error;

use super::wire::MAX_DNS_WIRE_BYTES;

pub const TCP_FRAME_PREFIX_BYTES: usize = 2;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum TcpFrameError {
    #[error("TCP DNS frame length must be greater than zero")]
    Empty,
    #[error("TCP DNS frame exceeds the {limit} byte limit")]
    TooLarge { limit: usize },
}

/// 解码网络序 length prefix；零长度 frame 不表示 EOF，而是协议错误。
pub fn decode_frame_length(prefix: [u8; TCP_FRAME_PREFIX_BYTES]) -> Result<usize, TcpFrameError> {
    let length = u16::from_be_bytes(prefix) as usize;
    if length == 0 {
        return Err(TcpFrameError::Empty);
    }
    Ok(length)
}

/// 为 DNS wire payload 添加两字节网络序长度前缀。
pub fn encode_frame(payload: &[u8], max_bytes: usize) -> Result<Vec<u8>, TcpFrameError> {
    let limit = max_bytes.min(MAX_DNS_WIRE_BYTES);
    if payload.is_empty() {
        return Err(TcpFrameError::Empty);
    }
    if payload.len() > limit {
        return Err(TcpFrameError::TooLarge { limit });
    }

    let length = u16::try_from(payload.len()).map_err(|_| TcpFrameError::TooLarge {
        limit: MAX_DNS_WIRE_BYTES,
    })?;
    let mut frame = Vec::with_capacity(TCP_FRAME_PREFIX_BYTES + payload.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::{MAX_DNS_WIRE_BYTES, TcpFrameError, decode_frame_length, encode_frame};

    #[test]
    fn encodes_and_decodes_network_order_length_prefix() {
        let payload = b"dns";
        let frame = encode_frame(payload, 512).unwrap();

        assert_eq!(frame[..2], [0, 3]);
        assert_eq!(decode_frame_length(frame[..2].try_into().unwrap()), Ok(3));
        assert_eq!(&frame[2..], payload);
    }

    #[test]
    fn rejects_empty_frame_and_payload() {
        assert_eq!(decode_frame_length([0, 0]), Err(TcpFrameError::Empty));
        assert_eq!(encode_frame(&[], 512), Err(TcpFrameError::Empty));
    }

    #[test]
    fn enforces_caller_limit_and_absolute_dns_limit() {
        assert_eq!(
            encode_frame(&[0_u8; 513], 512),
            Err(TcpFrameError::TooLarge { limit: 512 })
        );
        assert_eq!(
            encode_frame(&vec![0_u8; MAX_DNS_WIRE_BYTES + 1], MAX_DNS_WIRE_BYTES + 1),
            Err(TcpFrameError::TooLarge {
                limit: MAX_DNS_WIRE_BYTES
            })
        );
    }
}
