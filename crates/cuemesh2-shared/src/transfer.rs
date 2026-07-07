//! Binary chunk framing for controller→client media transfer.
//!
//! Media files ride the existing WebSocket as binary frames between a
//! `MEDIA_PUSH_BEGIN` and `MEDIA_PUSH_END` JSON envelope. Each binary frame
//! is `[u64 BE transfer_id][payload bytes]` so a receiver can associate
//! chunks with the announced transfer even if control messages interleave.

/// Chunk payload size. 256 KiB keeps frames small enough to interleave
/// heartbeats and status while still moving ~10 MB/s per client on
/// consumer Wi-Fi without hammering the event loop.
pub const CHUNK_SIZE: usize = 256 * 1024;

/// Frame a chunk: 8-byte big-endian transfer id, then the payload.
pub fn encode_chunk(transfer_id: u64, data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 + data.len());
    buf.extend_from_slice(&transfer_id.to_be_bytes());
    buf.extend_from_slice(data);
    buf
}

/// Parse a framed chunk. Returns `None` when the frame is too short.
pub fn decode_chunk(frame: &[u8]) -> Option<(u64, &[u8])> {
    if frame.len() < 8 {
        return None;
    }
    let mut id_bytes = [0u8; 8];
    id_bytes.copy_from_slice(&frame[..8]);
    Some((u64::from_be_bytes(id_bytes), &frame[8..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let frame = encode_chunk(42, b"hello");
        let (id, data) = decode_chunk(&frame).unwrap();
        assert_eq!(id, 42);
        assert_eq!(data, b"hello");
    }

    #[test]
    fn empty_payload_is_valid() {
        let frame = encode_chunk(7, b"");
        let (id, data) = decode_chunk(&frame).unwrap();
        assert_eq!(id, 7);
        assert!(data.is_empty());
    }

    #[test]
    fn short_frame_rejected() {
        assert!(decode_chunk(&[1, 2, 3]).is_none());
    }
}
