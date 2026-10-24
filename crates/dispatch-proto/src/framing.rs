//! Reliable-stream framing on the node link: `u32 big-endian length` + payload.
//! Matches FlowStation's `QuicTransport` (`network/transports/quic.rs`, `drain_length_prefixed`).

/// Largest frame we accept. Our messages are tiny; FlowStation itself allows 1 MiB.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_BYTES} byte limit (stream desynced?)")]
    TooLarge(usize),
}

/// Prefix `payload` with its length.
pub fn encode_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Accumulates bytes from a stream and yields complete frames.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Next complete frame, `Ok(None)` if more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME_BYTES {
            self.buf.clear();
            return Err(FrameError::TooLarge(len));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let frame = self.buf[4..4 + len].to_vec();
        self.buf.drain(..4 + len);
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_joined_frames() {
        let mut wire = encode_frame(b"hello");
        wire.extend(encode_frame(b""));
        wire.extend(encode_frame(b"world"));
        let mut dec = FrameDecoder::new();
        // Feed one byte at a time: framing must survive arbitrary read boundaries.
        let mut out = Vec::new();
        for b in wire {
            dec.push(&[b]);
            while let Some(f) = dec.next_frame().unwrap() {
                out.push(f);
            }
        }
        assert_eq!(out, vec![b"hello".to_vec(), vec![], b"world".to_vec()]);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut dec = FrameDecoder::new();
        dec.push(&((MAX_FRAME_BYTES as u32 + 1).to_be_bytes()));
        assert_eq!(dec.next_frame(), Err(FrameError::TooLarge(MAX_FRAME_BYTES + 1)));
    }
}
