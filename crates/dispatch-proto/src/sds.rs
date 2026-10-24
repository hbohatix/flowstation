//! Text SDS payload as FlowStation's `ControlCommand::SendSds` expects it.
//!
//! The node wraps it into SDS-TL simple text (protocol id 0x82, EN 300 392-2 §29.5.3) and uses
//! an optional leading text-coding-scheme octet (§29.5.4.1): none = ISO-8859-1 (scheme 0x01),
//! `0x02` = UTF-16BE. Text that fits Latin-1 is sent as Latin-1 (most compatible with radios);
//! anything else (Polish ą/ę/ł, Ukrainian Cyrillic, …) as UTF-16BE so it arrives intact.

/// Longest text we send. SDS type-4 user data is at most 2047 bits; SDS-TL adds 4 octets,
/// so 250 octets of payload are always safe — that is 250 Latin-1 or 124 UTF-16 characters.
pub const MAX_PAYLOAD_OCTETS: usize = 250;

/// Text coding scheme octet for UTF-16BE (EN 300 392-2 table 29.29).
pub const CODING_UTF16BE: u8 = 0x02;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SdsError {
    #[error("SDS text is empty")]
    Empty,
    #[error("SDS text is too long ({0} octets, max {MAX_PAYLOAD_OCTETS})")]
    TooLong(usize),
}

/// Encode `text` for `ControlCommand::SendSds { payload, len_bits }`.
pub fn encode_text(text: &str) -> Result<Vec<u8>, SdsError> {
    if text.is_empty() {
        return Err(SdsError::Empty);
    }
    let payload: Vec<u8> = if text.chars().all(|c| (c as u32) <= 0xFF) {
        text.chars().map(|c| c as u32 as u8).collect()
    } else {
        let mut out = vec![CODING_UTF16BE];
        for unit in text.encode_utf16() {
            out.extend_from_slice(&unit.to_be_bytes());
        }
        out
    };
    if payload.len() > MAX_PAYLOAD_OCTETS {
        return Err(SdsError::TooLong(payload.len()));
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin1_stays_single_byte() {
        assert_eq!(encode_text("Hello 10").unwrap(), b"Hello 10".to_vec());
        assert_eq!(encode_text("Grüße").unwrap(), vec![b'G', b'r', 0xFC, 0xDF, b'e']);
    }

    #[test]
    fn polish_and_ukrainian_use_utf16() {
        let p = encode_text("Łódź").unwrap();
        assert_eq!(p[0], CODING_UTF16BE);
        assert_eq!(&p[1..3], &[0x01, 0x41], "Ł = U+0141");
        let u = encode_text("Київ").unwrap();
        assert_eq!(u.len(), 1 + 4 * 2);
        assert_eq!(&u[1..3], &[0x04, 0x1A], "К = U+041A");
    }

    #[test]
    fn limits() {
        assert_eq!(encode_text(""), Err(SdsError::Empty));
        assert!(encode_text(&"a".repeat(250)).is_ok());
        assert_eq!(encode_text(&"a".repeat(251)), Err(SdsError::TooLong(251)));
        assert!(encode_text(&"ą".repeat(124)).is_ok());
        assert!(encode_text(&"ą".repeat(125)).is_err());
    }
}
