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
    let (ok, text) = run(&["definitely-not-a-role"]);
    assert!(!ok, "an unknown role must exit non-zero");
    assert!(
        text.contains("unknown role") && text.contains("definitely-not-a-role"),
        "the error names the offending role: {text}"
    );
    assert!(
        text.contains("acp") && text.contains("hand") && text.contains("memoryd"),
        "the error lists the valid roles: {text}"
    );
}

#[test]
fn no_role_prints_usage_and_fails() {
    let (ok, text) = run(&[]);
    assert!(!ok, "no role must exit non-zero");
    assert!(
        text.contains("usage") && text.contains("acp|hand|memoryd"),
        "with no role the binary prints usage: {text}"
    );
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

/// A thin build (no `memoryd` feature) stubs the `memoryd` role likewise.
#[cfg(not(feature = "memoryd"))]
#[test]
fn the_memoryd_role_stub_demands_its_feature_when_built_thin() {
    let (ok, text) = run(&["memoryd"]);
    assert!(!ok, "the memoryd stub must exit non-zero");
    assert!(
        text.contains("memoryd") && text.contains("--features memoryd"),
        "the thin build tells the operator to rebuild with --features memoryd: {text}"
    );
}
