//! `CancellationToken` + `CancellationHandle` pair — pins the surface
//! `reference/cancellation.md` cites for external-cancel control.

use awaken::CancellationToken;

fn main() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());

    // Cloning shares the same flag.
    let mirror = token.clone();
    token.cancel();
    assert!(mirror.is_cancelled());

    // Handle/Token pair separates the cancel-author from the observer.
    let (handle, observer) = CancellationToken::new_pair();
    assert!(!observer.is_cancelled());
    handle.cancel();
    assert!(observer.is_cancelled());
}
