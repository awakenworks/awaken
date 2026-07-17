//! Formal (property-based) verification of `validate_path_len` — the memory-store path
//! bound every backend enforces (ADR-0059 verification pass). The unit tests sample a few
//! lengths; this asserts the accept/reject law over ALL strings the generator produces,
//! pinning the exact `> MAX_PATH_BYTES` boundary (1024 accepted, 1025 rejected).

use awaken_resource_contract::{MAX_PATH_BYTES, validate_path_len};
use proptest::prelude::*;

proptest! {
    /// ACCEPT/REJECT LAW: a path validates iff its UTF-8 byte length is within the cap.
    /// Byte length (not char count) is what the bound uses, so multibyte inputs are
    /// exercised too.
    #[test]
    fn validates_iff_within_the_byte_cap(path in "\\PC{0,2100}") {
        let ok = validate_path_len(&path).is_ok();
        prop_assert_eq!(ok, path.len() <= MAX_PATH_BYTES,
            "validate_path_len disagreed with `len <= MAX` at len={}", path.len());
    }

    /// EXACT BOUNDARY: a path of exactly MAX bytes is accepted; MAX+1 is rejected. Pins the
    /// off-by-one a `>=`-vs-`>` refactor of the predicate could flip.
    #[test]
    fn the_boundary_is_exact(pad in 0usize..8) {
        let at_cap = "a".repeat(MAX_PATH_BYTES);
        prop_assert!(validate_path_len(&at_cap).is_ok(), "exactly MAX bytes must be accepted");
        let over = "a".repeat(MAX_PATH_BYTES + 1 + pad);
        prop_assert!(validate_path_len(&over).is_err(), "over MAX bytes must be rejected");
    }
}
