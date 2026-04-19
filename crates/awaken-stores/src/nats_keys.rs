//! NATS subject and KV-key segment encoding.
//!
//! Thread IDs and run IDs can be user-controlled. Encode them before embedding
//! in NATS subjects or KV keys so wildcard tokens and separators cannot change
//! routing semantics.

const PREFIX: char = 'h';
const HEX: &[u8; 16] = b"0123456789abcdef";

pub(crate) fn encode_segment(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(1 + bytes.len() * 2);
    out.push(PREFIX);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn decode_segment(value: &str) -> Option<String> {
    let encoded = value.strip_prefix(PREFIX)?;
    if encoded.len() % 2 != 0 {
        return None;
    }

    let mut bytes = Vec::with_capacity(encoded.len() / 2);
    for pair in encoded.as_bytes().chunks_exact(2) {
        let high = from_hex(pair[0])?;
        let low = from_hex(pair[1])?;
        bytes.push((high << 4) | low);
    }
    String::from_utf8(bytes).ok()
}

fn from_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_encoding_roundtrips_arbitrary_subject_chars() {
        let raw = "thread.*.with.>.and spaces/utf8-\u{4e2d}";
        let encoded = encode_segment(raw);

        assert!(!encoded.contains('*'));
        assert!(!encoded.contains('>'));
        assert!(!encoded.contains('.'));
        assert!(!encoded.contains(' '));
        assert_eq!(decode_segment(&encoded).as_deref(), Some(raw));
    }

    #[test]
    fn empty_segment_roundtrips_to_prefixed_token() {
        let encoded = encode_segment("");

        assert_eq!(encoded, "h");
        assert_eq!(decode_segment(&encoded).as_deref(), Some(""));
    }

    #[test]
    fn malformed_segments_return_none() {
        assert!(decode_segment("raw").is_none());
        assert!(decode_segment("h0").is_none());
        assert!(decode_segment("hzz").is_none());
    }
}
