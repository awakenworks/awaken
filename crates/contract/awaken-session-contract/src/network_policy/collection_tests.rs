//! Executable production collection checks against an independent ASCII host bitmap.
//! Domain: every policy tag, zero to two one-byte ASCII host strings, including
//! NUL, controls, whitespace, case aliases, duplicates and either ordering.
//! Real String/Vec sorting, deduplication and all owned Drop paths remain enabled.
//! This does not cover arbitrary UTF-8 strings, unbounded vectors, DNS or I/O.

use super::SessionNetworkPolicy;

fn raw_policy(kind: u8, length: u8, bytes: [u8; 2]) -> SessionNetworkPolicy {
    assert!(kind < 3 && length <= 2 && bytes[0] < 128 && bytes[1] < 128);
    match kind {
        0 => SessionNetworkPolicy::None,
        1 => SessionNetworkPolicy::Unrestricted,
        _ => {
            let mut hosts = Vec::new();
            for byte in &bytes[..usize::from(length)] {
                hosts.push(String::from(char::from(*byte)));
            }
            SessionNetworkPolicy::Allowlist { hosts }
        }
    }
}

fn expected_hosts(kind: u8, length: u8, bytes: [u8; 2]) -> u128 {
    if kind == 0 {
        return 0;
    }
    if kind == 1 {
        return u128::MAX;
    }
    let mut set = 0;
    for byte in &bytes[..usize::from(length)] {
        // For a one-byte string Unicode White_Space is 09..0D or 20.
        // In particular 0B is blank for str::trim, while NUL is a real byte.
        // ASCII case folding maps 41..5A to 61..7A. Arithmetic and a bitmap
        // specify this domain without calling trim/lowercase/sort/set helpers.
        if matches!(*byte, 9..=13 | 32) {
            continue;
        }
        let canonical = if (65..=90).contains(byte) {
            *byte + 32
        } else {
            *byte
        };
        set |= 1_u128 << canonical;
    }
    set
}

fn check_result(result: &SessionNetworkPolicy, unrestricted: bool, expected: u128) {
    // Decision table: R1 top is represented only by Unrestricted; R2 the empty
    // host set is represented only by None; R3 every other result is the exact
    // ascending unique list of the bitmap's bytes. R3 rejects duplicates, case
    // aliases, blanks, dropped hosts, extra hosts and unsorted binary-search data.
    if unrestricted {
        assert!(matches!(result, SessionNetworkPolicy::Unrestricted));
    } else if expected == 0 {
        assert!(matches!(result, SessionNetworkPolicy::None));
    } else {
        let SessionNetworkPolicy::Allowlist { hosts } = result else {
            panic!("nonempty restricted host set must remain an allowlist");
        };
        assert!(!hosts.is_empty() && hosts.len() <= 2);
        let mut observed = 0;
        let mut previous = None;
        for host in hosts {
            assert_eq!(host.len(), 1);
            let byte = host.as_bytes()[0];
            assert!(byte < 128);
            if let Some(previous) = previous {
                assert!(previous < byte, "canonical host order is strict");
            }
            previous = Some(byte);
            observed |= 1_u128 << byte;
        }
        assert_eq!(
            observed, expected,
            "result grants exactly the specified hosts"
        );
    }
}

fn check_normalization(kind: u8, length: u8, bytes: [u8; 2]) {
    let policy = raw_policy(kind, length, bytes);
    check_result(
        &policy.normalized(),
        kind == 1,
        expected_hosts(kind, length, bytes),
    );
}

fn check_intersection(left: (u8, u8, [u8; 2]), right: (u8, u8, [u8; 2])) {
    // Causes: each policy tag, slot count, byte normalization, duplicates/order
    // and membership in either operand. R4 either None/blank-only set absorbs;
    // R5 top is identity; R6 two allowlists grant exactly their shared bytes.
    // The independent bitmap intersection and R1..R3 check the exact returned
    // production value, not a copied collection algorithm or scalar selector.
    let expected =
        expected_hosts(left.0, left.1, left.2) & expected_hosts(right.0, right.1, right.2);
    let result = raw_policy(left.0, left.1, left.2)
        .safe_intersection(&raw_policy(right.0, right.1, right.2));
    check_result(&result, left.0 == 1 && right.0 == 1, expected);
}

#[cfg(test)]
fn cases(alphabet: &[u8]) -> Vec<(u8, u8, [u8; 2])> {
    let mut cases = vec![(0, 0, [0, 0]), (1, 0, [0, 0]), (2, 0, [0, 0])];
    for first in alphabet {
        cases.push((2, 1, [*first, 0]));
        for second in alphabet {
            cases.push((2, 2, [*first, *second]));
        }
    }
    cases
}

#[test]
fn normalization_exhausts_two_ascii_host_slots() {
    // R1..R3: exhaust all 128 ASCII bytes in both occupied slots, all shorter
    // lists and the two non-list tags: 16,515 distinct input policies.
    let inputs = cases(&(0..128).collect::<Vec<u8>>());
    assert_eq!(inputs.len(), 16_515);
    for (kind, length, bytes) in inputs {
        check_normalization(kind, length, bytes);
    }
}

#[test]
fn intersection_crosses_tag_order_alias_and_blank_rules() {
    // R1..R6: 159 policies over twelve representative ASCII bytes, crossed with
    // every other policy = 25,281 ordered pairs. Covers NUL vs blank, vertical
    // tab, case-only equality, distinct hosts, duplicates and reversed inputs.
    // The whole 0..127 intersection domain is not an executed Kani proof.
    let inputs = cases(&[0, 9, 10, 11, 13, 32, b'A', b'a', b'B', b'b', b'_', 127]);
    assert_eq!(inputs.len(), 159);
    for left in &inputs {
        for right in &inputs {
            check_intersection(*left, *right);
        }
    }
}
