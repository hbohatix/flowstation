use bitcode::{Decode, Encode};

#[derive(Debug, thiserror::Error)]
#[error("cannot decode message: {0}")]
pub struct CodecError(String);

/// Binary encoding used on the node link (same `bitcode` format as FlowStation's `net_control`).
pub fn encode<T: Encode + ?Sized>(value: &T) -> Vec<u8> {
    bitcode::encode(value)
}

pub fn decode<'a, T: Decode<'a>>(bytes: &'a [u8]) -> Result<T, CodecError> {
    bitcode::decode(bytes).map_err(|e| CodecError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NodeToCore;

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(decode::<NodeToCore>(&[0xff, 0x00, 0x13]).is_err());
    }
}
