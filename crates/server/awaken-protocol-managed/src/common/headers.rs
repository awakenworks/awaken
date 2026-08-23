//! Canonical public Managed API header vocabulary.

/// The Managed Agents beta wire header accepted by every Managed endpoint.
pub const MANAGED_BETA: &str = "managed-agents-2026-04-01";
/// User Profiles beta emitted by the pinned SDK 0.117.1. It remains accepted
/// for the legacy `relationship` vocabulary.
pub const USER_PROFILES_BETA: &str = "user-profiles-2026-03-24";
/// User Profiles beta emitted automatically by reviewed SDK 0.120.0. This is
/// the same resource family with the `access_type` vocabulary.
pub(crate) const USER_PROFILES_BETA_LATEST: &str = "user-profiles-2026-08-18";
/// Research-preview MCP Tunnel API beta.
pub const TUNNELS_BETA: &str = "mcp-tunnels-2026-06-22";
/// Deprecated organization-scoped Tunnel beta retained during the official
/// migration window. It is accepted only on `/v1/organizations/tunnels`.
pub const LEGACY_TUNNELS_BETA: &str = "mcp-tunnels-2026-05-19";

const MAX_IDEMPOTENCY_KEY_BYTES: usize = 255;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdempotencyKeyScan {
    /// No byte other than an allowed ASCII space has been observed.
    Blank,
    /// At least one visible non-space ASCII byte has been observed.
    Graphic,
    /// A control or non-ASCII byte has been observed. This state is absorbing.
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdempotencyKeyAdmission {
    Accepted,
    EmptyOrBlank,
    TooLong,
    InvalidByte,
}

/// One total transition for the byte-level admission language. Keeping this
/// independent of `HeaderValue` makes the product rule directly model
/// checkable; HTTP parsing remains an adapter assumption around this kernel.
#[must_use]
fn scan_idempotency_key_byte(state: IdempotencyKeyScan, byte: u8) -> IdempotencyKeyScan {
    if state == IdempotencyKeyScan::Invalid || !(0x20..=0x7e).contains(&byte) {
        IdempotencyKeyScan::Invalid
    } else if state == IdempotencyKeyScan::Graphic || byte != b' ' {
        IdempotencyKeyScan::Graphic
    } else {
        IdempotencyKeyScan::Blank
    }
}

/// Reject precisely the lengths outside the product's 1..=255 byte domain.
#[must_use]
const fn idempotency_key_length_rejection(length: usize) -> Option<IdempotencyKeyAdmission> {
    if length == 0 {
        Some(IdempotencyKeyAdmission::EmptyOrBlank)
    } else if length > MAX_IDEMPOTENCY_KEY_BYTES {
        Some(IdempotencyKeyAdmission::TooLong)
    } else {
        None
    }
}

#[must_use]
const fn idempotency_key_scan_admission(scan: IdempotencyKeyScan) -> IdempotencyKeyAdmission {
    match scan {
        IdempotencyKeyScan::Blank => IdempotencyKeyAdmission::EmptyOrBlank,
        IdempotencyKeyScan::Graphic => IdempotencyKeyAdmission::Accepted,
        IdempotencyKeyScan::Invalid => IdempotencyKeyAdmission::InvalidByte,
    }
}

/// Decide the exact repository-owned admission language: one to 255 visible
/// ASCII bytes with at least one non-space byte. The scan does not normalize or
/// trim the key, so an accepted key retains its wire identity exactly.
#[must_use]
fn idempotency_key_admission(bytes: &[u8]) -> IdempotencyKeyAdmission {
    if let Some(rejection) = idempotency_key_length_rejection(bytes.len()) {
        return rejection;
    }
    let scan = bytes
        .iter()
        .copied()
        .fold(IdempotencyKeyScan::Blank, scan_idempotency_key_byte);
    idempotency_key_scan_admission(scan)
}

/// Parse the shared mutation replay header once for Managed and Awaken-owned
/// control commands. The caller owns its protocol-specific error envelope.
pub fn parse_idempotency_key_header(
    headers: &axum::http::HeaderMap,
) -> Result<Option<String>, &'static str> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .map_err(|_| "Idempotency-Key must be visible ASCII")?;
    match idempotency_key_admission(key.as_bytes()) {
        IdempotencyKeyAdmission::Accepted => Ok(Some(key.to_owned())),
        IdempotencyKeyAdmission::EmptyOrBlank | IdempotencyKeyAdmission::TooLong => {
            Err("Idempotency-Key must contain 1 to 255 characters")
        }
        IdempotencyKeyAdmission::InvalidByte => Err("Idempotency-Key must be visible ASCII"),
    }
}

#[cfg(kani)]
fn arbitrary_idempotency_key_scan(bits: u8) -> IdempotencyKeyScan {
    match bits % 3 {
        0 => IdempotencyKeyScan::Blank,
        1 => IdempotencyKeyScan::Graphic,
        _ => IdempotencyKeyScan::Invalid,
    }
}

/// Exhausts every prior summary and every byte. This is the inductive step for
/// arbitrary finite keys: invalid is absorbing, while `Graphic` is reached
/// exactly when all bytes are visible and one byte is not an ASCII space.
#[cfg(kani)]
#[kani::proof]
fn idempotency_key_scan_transition_is_exact_and_invalid_absorbing() {
    let state = arbitrary_idempotency_key_scan(kani::any());
    let byte: u8 = kani::any();
    let next = scan_idempotency_key_byte(state, byte);
    let visible = (0x20..=0x7e).contains(&byte);

    assert_eq!(
        next == IdempotencyKeyScan::Invalid,
        state == IdempotencyKeyScan::Invalid || !visible
    );
    assert_eq!(
        next == IdempotencyKeyScan::Graphic,
        state != IdempotencyKeyScan::Invalid
            && visible
            && (state == IdempotencyKeyScan::Graphic || byte != b' ')
    );
    if state == IdempotencyKeyScan::Invalid {
        assert_eq!(next, IdempotencyKeyScan::Invalid);
    }
}

/// Proves the complete terminal policy independently of iteration: precisely
/// lengths 1..=255 proceed to scanning, and only the `Graphic` summary admits.
#[cfg(kani)]
#[kani::proof]
fn idempotency_key_length_and_summary_policy_is_exact() {
    let length: usize = kani::any();
    let scan = arbitrary_idempotency_key_scan(kani::any());
    let length_rejection = idempotency_key_length_rejection(length);
    let scan_admission = idempotency_key_scan_admission(scan);

    assert_eq!(
        length_rejection.is_none(),
        (1..=MAX_IDEMPOTENCY_KEY_BYTES).contains(&length)
    );
    assert_eq!(
        scan_admission == IdempotencyKeyAdmission::Accepted,
        scan == IdempotencyKeyScan::Graphic
    );
    assert_eq!(
        scan_admission == IdempotencyKeyAdmission::InvalidByte,
        scan == IdempotencyKeyScan::Invalid
    );
}

/// Exercises the production fold at each fail-closed boundary. Together with
/// the exhaustive transition proof above, this establishes the accepted byte
/// language for keys of any length without imposing a smaller proof-only bound.
#[cfg(kani)]
#[kani::proof]
fn idempotency_key_admission_rejects_empty_overlong_and_every_invalid_byte() {
    let byte: u8 = kani::any();
    let singleton = idempotency_key_admission(&[byte]);
    assert_eq!(
        singleton == IdempotencyKeyAdmission::Accepted,
        (0x21..=0x7e).contains(&byte)
    );
    if !(0x20..=0x7e).contains(&byte) {
        assert_eq!(singleton, IdempotencyKeyAdmission::InvalidByte);
    }

    assert_eq!(
        idempotency_key_admission(&[]),
        IdempotencyKeyAdmission::EmptyOrBlank
    );
    assert_eq!(
        idempotency_key_admission(&[b'k'; MAX_IDEMPOTENCY_KEY_BYTES + 1]),
        IdempotencyKeyAdmission::TooLong
    );
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::*;

    fn headers_with(value: HeaderValue) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", value);
        headers
    }

    #[test]
    fn idempotency_key_adapter_preserves_missing_and_accepted_wire_identity() {
        assert_eq!(parse_idempotency_key_header(&HeaderMap::new()), Ok(None));

        let key = " replay /:_-.09~ ";
        assert_eq!(
            parse_idempotency_key_header(&headers_with(HeaderValue::from_static(key))),
            Ok(Some(key.to_owned()))
        );
    }

    #[test]
    fn idempotency_key_adapter_enforces_exact_length_boundaries() {
        let maximum = "k".repeat(MAX_IDEMPOTENCY_KEY_BYTES);
        assert_eq!(
            parse_idempotency_key_header(&headers_with(
                HeaderValue::from_str(&maximum).expect("visible ASCII header")
            )),
            Ok(Some(maximum))
        );

        let overlong = "k".repeat(MAX_IDEMPOTENCY_KEY_BYTES + 1);
        assert_eq!(
            parse_idempotency_key_header(&headers_with(
                HeaderValue::from_str(&overlong).expect("visible ASCII header")
            )),
            Err("Idempotency-Key must contain 1 to 255 characters")
        );
    }

    #[test]
    fn idempotency_key_admission_fails_closed_for_blank_and_invalid_bytes() {
        for blank in ["", " ", "    "] {
            assert_eq!(
                parse_idempotency_key_header(&headers_with(
                    HeaderValue::from_str(blank).expect("legal HTTP header")
                )),
                Err("Idempotency-Key must contain 1 to 255 characters")
            );
        }

        for byte in u8::MIN..=u8::MAX {
            let admitted = idempotency_key_admission(&[byte]);
            assert_eq!(
                admitted == IdempotencyKeyAdmission::Accepted,
                (0x21..=0x7e).contains(&byte),
                "unexpected singleton admission for byte {byte:#04x}"
            );
        }

        let non_ascii = HeaderValue::from_bytes(&[0x80]).expect("obs-text header value");
        assert_eq!(
            parse_idempotency_key_header(&headers_with(non_ascii)),
            Err("Idempotency-Key must be visible ASCII")
        );
    }
}
