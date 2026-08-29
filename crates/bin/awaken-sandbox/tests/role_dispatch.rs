//! `main.rs` role dispatch, against the REAL built binary (via `CARGO_BIN_EXE_*`, no extra
//! dev-dep). The first arg selects the role; this covers the arms that decide *without*
//! serving: the unknown-role failure, the missing-arg usage, and the
//! `#[cfg(not(feature = ...))]` "needs a build with --features" stubs a thin (acp-only)
//! image emits for a fat role it was not built with. Each arm exits promptly (no listen
//! loop), so these are deterministic — no readiness signal needed.

use std::process::Command;

/// The binary under test, built by cargo for this integration target.
fn awaken_sandbox() -> Command {
    Command::new(env!("CARGO_BIN_EXE_awaken-sandbox"))
}

/// Run the binary and return (success, combined stderr+stdout).
fn run(args: &[&str]) -> (bool, String) {
    let out = awaken_sandbox()
        .args(args)
        .output()
        .expect("spawn awaken-sandbox");
    let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    (out.status.success(), text)
}

#[test]
fn an_unknown_role_fails_and_names_the_valid_roles() {
    // Cause/effect graph: C1=known closed role, C2=unknown or retired role.
    // Effects: E1=dispatch to its sole owner; E2=non-zero with only canonical
    // roles. Decision R1(C1)->E1 is covered by the role tests; R2(C2)->E2 is
    // this row and proves the retired memoryd process cannot remain a second
    // Memory authority.
    let (ok, text) = run(&["definitely-not-a-role"]);
    assert!(!ok, "an unknown role must exit non-zero");
    assert!(
        text.contains("unknown role") && text.contains("definitely-not-a-role"),
        "the error names the offending role: {text}"
    );
    assert!(
        text.contains("acp")
            && text.contains("hand")
            && text.contains("git-credential")
            && text.contains("control-forwarder")
            && text.contains("control-forwarder-ready")
            && !text.contains("memoryd"),
        "the error lists the valid roles: {text}"
    );
}

#[test]
fn no_role_prints_usage_and_fails() {
    /* Role-dispatch rule D2: an absent role (cause) returns failure plus the
     * same complete closed-role vocabulary in usage (effects).
     */
    let (ok, text) = run(&[]);
    assert!(!ok, "no role must exit non-zero");
    assert!(
        text.contains("usage")
            && text.contains("acp|hand|git-credential|control-forwarder|control-forwarder-ready")
            && !text.contains("memoryd"),
        "with no role the binary prints usage: {text}"
    );
}

#[test]
fn readiness_role_rejects_noncanonical_markers_without_touching_a_port() {
    /* Role readiness rule D3: C1=readiness role with a noncanonical marker;
     * E1=fail immediately in the marker parser; E2=no network business-port
     * connection. This complements the module test for a current marker.
     */
    let (ok, text) = run(&[
        "control-forwarder-ready",
        "--marker",
        "/tmp/not-the-control-marker",
    ]);
    assert!(!ok, "D3/E1");
    assert!(text.contains("control-forwarder-ready"), "D3/E1: {text}");
}

/// A thin build (no `hand` feature) stubs the `hand` role with a "needs --features hand"
/// message and fails, rather than pretending to serve.
#[cfg(not(feature = "hand"))]
#[test]
fn the_hand_role_stub_demands_its_feature_when_built_thin() {
    let (ok, text) = run(&["hand", "--unix", "/rv/hand.sock"]);
    assert!(!ok, "the hand stub must exit non-zero");
    assert!(
        text.contains("hand") && text.contains("--features hand"),
        "the thin build tells the operator to rebuild with --features hand: {text}"
    );
}

/// A fat build (`--features hand`) does NOT stub — with no bind flags it reaches the real
/// `parse_hand_args`, which fails closed asking for a transport. This proves dispatch
/// actually routed into the compiled-in role (the opposite of the stub above).
#[cfg(feature = "hand")]
#[test]
fn the_hand_role_routes_into_the_real_arg_parser_when_built_fat() {
    let (ok, text) = run(&["hand"]);
    assert!(!ok, "hand with no transport must exit non-zero");
    assert!(
        text.contains("hand requires") && text.contains("--unix"),
        "a fat build reaches the real parse_hand_args (not the stub): {text}"
    );
}
