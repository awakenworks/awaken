//! Executable production collection checks against an independent ASCII host bitmap.
//! Kani domain: every policy tag, zero to two one-byte ASCII host strings, including
//! NUL, controls, whitespace, case aliases, duplicates and either ordering.
//! Real String/Vec sorting, deduplication and all owned Drop paths remain enabled.
//! Native collection-size rows additionally cover zero to 257 one-byte hosts.
//! These finite domains do not cover arbitrary UTF-8 strings, unbounded vectors, DNS or I/O.

use super::SessionNetworkPolicy;

fn ascii_host(byte: u8) -> String {
    assert!(byte < 128);
    // The domain is already ASCII. Move its one-byte buffer into String after
    // UTF-8 validation instead of re-encoding a char.
    String::from_utf8(vec![byte]).expect("ASCII is valid UTF-8")
}

fn raw_policy(kind: u8, length: u8, bytes: [u8; 2]) -> SessionNetworkPolicy {
    assert!(kind < 3 && length <= 2 && bytes[0] < 128 && bytes[1] < 128);
    match kind {
        0 => SessionNetworkPolicy::None,
        1 => SessionNetworkPolicy::Unrestricted,
        _ => {
            let hosts = match length {
                0 => Vec::new(),
                1 => vec![ascii_host(bytes[0])],
                2 => vec![ascii_host(bytes[0]), ascii_host(bytes[1])],
                _ => unreachable!("the bounded domain was checked above"),
            };
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

fn check_result(
    result: &SessionNetworkPolicy,
    unrestricted: bool,
    expected: u128,
    max_hosts: usize,
) {
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
        assert!(!hosts.is_empty() && hosts.len() <= max_hosts);
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
        2,
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
    check_result(&result, left.0 == 1 && right.0 == 1, expected, 2);
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
fn bounded_input_construction_preserves_every_occupied_byte() {
    // G1 all tags and all 0..2-slot ASCII values -> exact raw policy tags,
    // byte lengths, byte identities and slot ordering before normalization.
    // G2 unoccupied slots contribute no host. Exhaust 16,515 distinct policies
    // so fixed-arity Vec construction cannot silently narrow or normalize the
    // Kani input domain, nor mask trim/folding/order errors in production.
    let inputs = cases(&(0..128).collect::<Vec<u8>>());
    assert_eq!(inputs.len(), 16_515);
    for (kind, length, bytes) in inputs {
        match raw_policy(kind, length, bytes) {
            SessionNetworkPolicy::None => assert_eq!(kind, 0),
            SessionNetworkPolicy::Unrestricted => assert_eq!(kind, 1),
            SessionNetworkPolicy::Allowlist { hosts } => {
                assert_eq!(kind, 2);
                assert_eq!(hosts.len(), usize::from(length));
                for (index, host) in hosts.iter().enumerate() {
                    assert_eq!(host.as_bytes(), &[bytes[index]]);
                }
            }
        }
    }
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
    // The Kani partitions below separately exhaust the full one-byte ASCII domain.
    let inputs = cases(&[0, 9, 10, 11, 13, 32, b'A', b'a', b'B', b'b', b'_', 127]);
    assert_eq!(inputs.len(), 159);
    for left in &inputs {
        for right in &inputs {
            check_intersection(*left, *right);
        }
    }
}

#[test]
fn canonical_collection_is_exact_across_sort_sizes_and_input_orders() {
    // Cause/effect table: C1 lengths straddle small/medium collection sizes;
    // C2 raw bytes contain blanks, case aliases, duplicates, NUL or DEL;
    // C3 input order is cyclic, ascending or descending. R7 every combination
    // -> the exact independent bitmap, strict byte order and no duplicates;
    // R8 blank-only or empty inputs -> None. Equal String keys have no payload
    // beyond their bytes, so stability adds no observable value after dedup.
    // This exercises real String/Vec sorting beyond the two-slot Kani bound;
    // the same R1..R3 oracle owns content/order/representation expectations.
    let mixed = [0, 9, 11, 32, b'A', b'a', b'B', b'b', b'_', 127];
    let blank = [9, 11, 32];
    let canonical = (b'a'..=b'z').collect::<Vec<_>>();
    for alphabet in [&mixed[..], &blank[..], canonical.as_slice()] {
        for length in [
            0, 1, 2, 19, 20, 21, 31, 32, 33, 63, 64, 65, 127, 128, 129, 257,
        ] {
            for order in 0..3 {
                let mut bytes = (0..length)
                    .map(|index| alphabet[index % alphabet.len()])
                    .collect::<Vec<_>>();
                if order != 0 {
                    bytes.sort_unstable();
                    if order == 2 {
                        bytes.reverse();
                    }
                }
                let expected = bytes
                    .iter()
                    .fold(0, |set, byte| set | expected_hosts(2, 1, [*byte, 0]));
                let policy = SessionNetworkPolicy::Allowlist {
                    hosts: bytes.into_iter().map(ascii_host).collect(),
                };
                check_result(&policy.normalized(), false, expected, length);
            }
        }
    }
}

// P0..P4 partition the complete existing bounded domain: the two non-list
// variants and list lengths 0, 1, 2. Unoccupied bytes do not enter a policy.
// Each partition calls the same production normalizer and independent R1..R3
// oracle. Only the conjunction covers all 16,515 policies; no single partition
// implies complete normalization or intersection coverage.
#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_normalization_none_partition() {
    // P0 -> R2: closed networking must remain the sole empty representation.
    check_normalization(0, 0, [0, 0]);
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_normalization_unrestricted_partition() {
    // P1 -> R1: top retains its exact enum representation.
    check_normalization(1, 0, [0, 0]);
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_normalization_empty_partition() {
    // P2 -> R2: an owned empty Vec normalizes to None, with real Drop enabled.
    check_normalization(2, 0, [0, 0]);
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_normalization_one_ascii_host_partition() {
    // P3 -> R2/R3: every occupied ASCII byte, including NUL, all whitespace,
    // case aliases and DEL, has exactly its independent bitmap semantics.
    let byte: u8 = kani::any();
    kani::assume(byte < 128);
    check_normalization(2, 1, [byte, 0]);
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_normalization_two_ascii_hosts_partition() {
    // P4 -> R2/R3: every ordered ASCII pair additionally covers two blanks,
    // equal/case-alias hosts, distinct hosts and either input ordering.
    let bytes: [u8; 2] = kani::any();
    kani::assume(bytes[0] < 128 && bytes[1] < 128);
    check_normalization(2, 2, bytes);
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_two_by_two_ascii_hosts_partition() {
    // I44 -> R2/R3/R6: all four occupied bytes vary independently across ASCII.
    // Both inputs include blanks, case aliases, duplicates and either order;
    // their real normalized collection intersection must equal bitmap AND.
    // Fixed slot counts preserve the input domain without assuming canonical
    // inputs or bypassing String/Vec sorting, binary search, filtering or Drop.
    let left: [u8; 2] = kani::any();
    let right: [u8; 2] = kani::any();
    kani::assume(left[0] < 128 && left[1] < 128);
    kani::assume(right[0] < 128 && right[1] < 128);
    check_intersection((2, 2, left), (2, 2, right));
}

// I00..I44 cross P0..P4 on the left and right. Their disjoint raw-policy
// domains contain (1 + 1 + 1 + 128 + 128^2)^2 = 272,745,225 ordered pairs.
// I44 above owns the largest partition; the 24 rows below complete the union.
// All rows execute the same owned production composition and R1..R6 oracle.

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_none_by_none_partition() {
    // I00 -> R1..R6: left none, right none; exact bitmap meet.
    let left = [0, 0];
    let right = [0, 0];
    check_intersection((0, 0, left), (0, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_none_by_unrestricted_partition() {
    // I01 -> R1..R6: left none, right unrestricted; exact bitmap meet.
    let left = [0, 0];
    let right = [0, 0];
    check_intersection((0, 0, left), (1, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_none_by_empty_partition() {
    // I02 -> R1..R6: left none, right empty; exact bitmap meet.
    let left = [0, 0];
    let right = [0, 0];
    check_intersection((0, 0, left), (2, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_none_by_one_ascii_host_partition() {
    // I03 -> R1..R6: left none, right one_ascii_host; exact bitmap meet.
    let left = [0, 0];
    let right_byte: u8 = kani::any();
    kani::assume(right_byte < 128);
    let right = [right_byte, 0];
    check_intersection((0, 0, left), (2, 1, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_none_by_two_ascii_hosts_partition() {
    // I04 -> R1..R6: left none, right two_ascii_hosts; exact bitmap meet.
    let left = [0, 0];
    let right: [u8; 2] = kani::any();
    kani::assume(right[0] < 128 && right[1] < 128);
    check_intersection((0, 0, left), (2, 2, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_unrestricted_by_none_partition() {
    // I10 -> R1..R6: left unrestricted, right none; exact bitmap meet.
    let left = [0, 0];
    let right = [0, 0];
    check_intersection((1, 0, left), (0, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_unrestricted_by_unrestricted_partition() {
    // I11 -> R1..R6: left unrestricted, right unrestricted; exact bitmap meet.
    let left = [0, 0];
    let right = [0, 0];
    check_intersection((1, 0, left), (1, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_unrestricted_by_empty_partition() {
    // I12 -> R1..R6: left unrestricted, right empty; exact bitmap meet.
    let left = [0, 0];
    let right = [0, 0];
    check_intersection((1, 0, left), (2, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_unrestricted_by_one_ascii_host_partition() {
    // I13 -> R1..R6: left unrestricted, right one_ascii_host; exact bitmap meet.
    let left = [0, 0];
    let right_byte: u8 = kani::any();
    kani::assume(right_byte < 128);
    let right = [right_byte, 0];
    check_intersection((1, 0, left), (2, 1, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_unrestricted_by_two_ascii_hosts_partition() {
    // I14 -> R1..R6: left unrestricted, right two_ascii_hosts; exact bitmap meet.
    let left = [0, 0];
    let right: [u8; 2] = kani::any();
    kani::assume(right[0] < 128 && right[1] < 128);
    check_intersection((1, 0, left), (2, 2, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_empty_by_none_partition() {
    // I20 -> R1..R6: left empty, right none; exact bitmap meet.
    let left = [0, 0];
    let right = [0, 0];
    check_intersection((2, 0, left), (0, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_empty_by_unrestricted_partition() {
    // I21 -> R1..R6: left empty, right unrestricted; exact bitmap meet.
    let left = [0, 0];
    let right = [0, 0];
    check_intersection((2, 0, left), (1, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_empty_by_empty_partition() {
    // I22 -> R1..R6: left empty, right empty; exact bitmap meet.
    let left = [0, 0];
    let right = [0, 0];
    check_intersection((2, 0, left), (2, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_empty_by_one_ascii_host_partition() {
    // I23 -> R1..R6: left empty, right one_ascii_host; exact bitmap meet.
    let left = [0, 0];
    let right_byte: u8 = kani::any();
    kani::assume(right_byte < 128);
    let right = [right_byte, 0];
    check_intersection((2, 0, left), (2, 1, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_empty_by_two_ascii_hosts_partition() {
    // I24 -> R1..R6: left empty, right two_ascii_hosts; exact bitmap meet.
    let left = [0, 0];
    let right: [u8; 2] = kani::any();
    kani::assume(right[0] < 128 && right[1] < 128);
    check_intersection((2, 0, left), (2, 2, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_one_ascii_host_by_none_partition() {
    // I30 -> R1..R6: left one_ascii_host, right none; exact bitmap meet.
    let left_byte: u8 = kani::any();
    kani::assume(left_byte < 128);
    let left = [left_byte, 0];
    let right = [0, 0];
    check_intersection((2, 1, left), (0, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_one_ascii_host_by_unrestricted_partition() {
    // I31 -> R1..R6: left one_ascii_host, right unrestricted; exact bitmap meet.
    let left_byte: u8 = kani::any();
    kani::assume(left_byte < 128);
    let left = [left_byte, 0];
    let right = [0, 0];
    check_intersection((2, 1, left), (1, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_one_ascii_host_by_empty_partition() {
    // I32 -> R1..R6: left one_ascii_host, right empty; exact bitmap meet.
    let left_byte: u8 = kani::any();
    kani::assume(left_byte < 128);
    let left = [left_byte, 0];
    let right = [0, 0];
    check_intersection((2, 1, left), (2, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_one_ascii_host_by_one_ascii_host_partition() {
    // I33 -> R1..R6: left one_ascii_host, right one_ascii_host; exact bitmap meet.
    let left_byte: u8 = kani::any();
    kani::assume(left_byte < 128);
    let left = [left_byte, 0];
    let right_byte: u8 = kani::any();
    kani::assume(right_byte < 128);
    let right = [right_byte, 0];
    check_intersection((2, 1, left), (2, 1, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_one_ascii_host_by_two_ascii_hosts_partition() {
    // I34 -> R1..R6: left one_ascii_host, right two_ascii_hosts; exact bitmap meet.
    let left_byte: u8 = kani::any();
    kani::assume(left_byte < 128);
    let left = [left_byte, 0];
    let right: [u8; 2] = kani::any();
    kani::assume(right[0] < 128 && right[1] < 128);
    check_intersection((2, 1, left), (2, 2, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_two_ascii_hosts_by_none_partition() {
    // I40 -> R1..R6: left two_ascii_hosts, right none; exact bitmap meet.
    let left: [u8; 2] = kani::any();
    kani::assume(left[0] < 128 && left[1] < 128);
    let right = [0, 0];
    check_intersection((2, 2, left), (0, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_two_ascii_hosts_by_unrestricted_partition() {
    // I41 -> R1..R6: left two_ascii_hosts, right unrestricted; exact bitmap meet.
    let left: [u8; 2] = kani::any();
    kani::assume(left[0] < 128 && left[1] < 128);
    let right = [0, 0];
    check_intersection((2, 2, left), (1, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_two_ascii_hosts_by_empty_partition() {
    // I42 -> R1..R6: left two_ascii_hosts, right empty; exact bitmap meet.
    let left: [u8; 2] = kani::any();
    kani::assume(left[0] < 128 && left[1] < 128);
    let right = [0, 0];
    check_intersection((2, 2, left), (2, 0, right));
}

#[cfg(kani)]
#[kani::proof]
#[kani::unwind(3)]
fn session_network_intersection_two_ascii_hosts_by_one_ascii_host_partition() {
    // I43 -> R1..R6: left two_ascii_hosts, right one_ascii_host; exact bitmap meet.
    let left: [u8; 2] = kani::any();
    kani::assume(left[0] < 128 && left[1] < 128);
    let right_byte: u8 = kani::any();
    kani::assume(right_byte < 128);
    let right = [right_byte, 0];
    check_intersection((2, 2, left), (2, 1, right));
}
