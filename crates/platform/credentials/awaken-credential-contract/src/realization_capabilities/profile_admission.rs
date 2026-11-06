// @formal entrypoint credential.material_profile_admission_preserves_complete_tuples
//! Complete-profile admission: dimensions from different adapters never join.

pub(crate) const fn credential_profile_supports(
    holder_matches: bool,
    source_matches: bool,
    mechanism_matches: bool,
    envelope_present: bool,
    envelope_supported: bool,
    envelope_required: bool,
) -> bool {
    holder_matches
        && source_matches
        && mechanism_matches
        && if envelope_present {
            envelope_supported
        } else {
            !envelope_required
        }
}

#[cfg(test)]
mod exhaustive {
    use super::credential_profile_supports;

    // Complete finite cause/effect domain: each profile has independent holder,
    // source, mechanism, envelope support/requirement; the request has one mode.
    // R1 any complete profile -> admit; R2 no complete profile -> reject, even
    // when every dimension appears somewhere in the union of profiles. R3 an
    // envelope-required profile never admits an unsealed request. This checks
    // the production kernel; actual profile composition/codec is tested in Rust.
    /// @exhaustive bool(2) x [bool; 5](32) x [bool; 5](32) = 2048
    /// @obligation credential.material_profile_admission_preserves_complete_tuples
    #[test]
    fn credential_profile_admission_requires_one_complete_material_tuple() {
        let profile = |mask: u8| -> [bool; 5] { std::array::from_fn(|bit| mask & (1 << bit) != 0) };
        for present in [false, true] {
            for a_mask in 0_u8..32 {
                for b_mask in 0_u8..32 {
                    let a = profile(a_mask);
                    let b = profile(b_mask);
                    let accepts = |p: [bool; 5]| {
                        credential_profile_supports(p[0], p[1], p[2], present, p[3], p[4])
                    };
                    let complete = |p: [bool; 5]| {
                        let envelope_matches = (present && p[3]) || (!present && !p[4]);
                        [p[0], p[1], p[2], envelope_matches]
                            .iter()
                            .all(|value| *value)
                    };
                    assert_eq!(accepts(a) || accepts(b), complete(a) || complete(b));
                    if !present && a[4] {
                        assert!(!accepts(a));
                    }
                    if !present && b[4] {
                        assert!(!accepts(b));
                    }
                }
            }
        }
    }
}
