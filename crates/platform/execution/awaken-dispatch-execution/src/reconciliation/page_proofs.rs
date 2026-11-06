//! Shared decision-table oracle for the production page iterator.
use super::TerminalScanPage;

// Causes: supplied page length; arbitrary limit; existing cursor; distinct or
// repeated identities; interruption position; whether the observer finishes
// after the last yield or is cancelled there. Effects: exact ordered
// prefix, min(limit,256) budget, advance before observer, and wrap only after
// exhausting a short positive-budget page. R0 limit=0 -> unchanged; R1 each
// yielded ID -> cursor equals that ID; R2 stop/drop before exhaustion -> retain
// last yielded ID; R3 exhausted short page -> None; R4 exhausted full/oversized
// page -> retain last ID. No callback result can affect this iterator, so error,
// missing, nonterminal and terminal observations share R1/R2. Async integration
// tests exercise those results with actual RunId and real queue adapters.
fn check_page(ids: &[u8], initial: Option<u8>, limit: usize, stop: usize, finish: bool) {
    let expected_budget = if limit > 256 { 256 } else { limit };
    let expected_count = ids.len().min(expected_budget);
    let mut cursor = initial;
    let exhausted = finish && stop >= expected_count;
    {
        let mut page = TerminalScanPage::new(ids, &mut cursor, limit);
        assert_eq!(page.budget, expected_budget);
        for (index, expected) in ids.iter().take(expected_count.min(stop)).enumerate() {
            assert_eq!(page.next(), Some(expected));
            assert_eq!(*page.after, Some(*expected), "R1 before observer");
            assert_eq!(page.visited, index + 1);
        }
        if exhausted {
            assert_eq!(page.next(), None, "budget terminates traversal");
            assert_eq!(page.next(), None, "exhaustion remains stable");
        }
    }
    let observed = expected_count.min(stop);
    let expected_cursor = if exhausted && expected_budget > ids.len() {
        None
    } else if observed == 0 {
        initial
    } else {
        Some(ids[observed - 1])
    };
    assert_eq!(cursor, expected_cursor, "R0/R2/R3/R4");
}

#[cfg(test)]
#[test]
fn terminal_scan_page_decision_table() {
    // Native boundary extension of the same oracle: empty, short, exact, and
    // oversized adapter pages against zero/one/255/256/257/MAX limits; early
    // cancellation, last item, and complete exhaustion. Repeated byte identities
    // deliberately prevent an ordering/uniqueness assumption from hiding bugs.
    let ids: Vec<u8> = (0..300).map(|index| (index % 251) as u8).collect();
    for len in [0, 1, 2, 255, 256, 257, 300] {
        for limit in [0, 1, 2, 255, 256, 257, usize::MAX] {
            for initial in [None, Some(7)] {
                for stop in [0, 1, 2, 255, 256, 257, 300] {
                    for finish in [false, true] {
                        check_page(&ids[..len], initial, limit, stop, finish);
                    }
                }
            }
        }
    }
}

#[cfg(kani)]
/// @obligation remote_worker.terminal_scan_is_bounded_and_preserves_authority
/// @kani default-features=false
#[kani::proof]
#[kani::unwind(5)]
fn terminal_scan_page_preserves_budget_cursor_and_cancellation() {
    // All byte identities (including duplicates), 0..3 supplied rows, all usize
    // limits, arbitrary initial cursor and every interruption position. Calls
    // the exact production iterator; no mock transition or proof-only branch.
    let ids: [u8; 3] = kani::any();
    let len: usize = kani::any();
    let stop: usize = kani::any();
    kani::assume(len <= ids.len());
    kani::assume(stop <= ids.len());
    check_page(&ids[..len], kani::any(), kani::any(), stop, kani::any());
}
