use awaken_credential_store::{BUNDLE_ID, credential_bundle};

#[test]
fn crate_root_exports_the_authoritative_credential_bundle() {
    // Cause/effect design (single-rule identity contract): C1 an external crate imports the
    // public constructor and BUNDLE_ID from the store root. E1 both names compile without
    // exposing the private schema module; E2 construction succeeds; E3 the returned bundle
    // carries that exact authoritative id. There are no interacting conditions, failures, or
    // state transitions, so one direct rule covers the complete public re-export behavior.
    let bundle = credential_bundle().expect("authoritative credential bundle builds");

    assert_eq!(bundle.bundle_id(), BUNDLE_ID);
}
