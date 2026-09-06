//! `main.rs` role dispatch, against the REAL built binary (via `CARGO_BIN_EXE_*`, no extra
//! dev-dep). The first arg selects the role; this covers the arms that decide *without*
//! serving: the unknown-role failure, the missing-arg usage, and the
//! `#[cfg(not(feature = ...))]` "needs a build with --features" stubs a thin (acp-only)
//! image emits for a fat role it was not built with. Each arm exits promptly (no listen
//! loop), so these are deterministic — no readiness signal needed.

use std::process::Command;

mod binary_support;

/// The binary under test, built by cargo for this integration target.
fn awaken_sandbox() -> Command {
    Command::new(binary_support::awaken_sandbox())
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
    // Harness causes/effects: H1 Cargo checks this target without a runnable
    // path -> it compiles; H2 Cargo runs it with the runtime path -> the exact
    // package binary is spawned. The workspace all-target check covers H1 and
    // every process-level test in this directory exercises H2 through the one
    // shared lookup.
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
            && text.contains("repository-publish-noreplace")
            && text.contains("read-tree-nofollow")
            && text.contains("channel-listen")
            && text.contains("channel-connect")
            && !text.contains("memoryd"),
        "the error lists the valid roles: {text}"
    );
}

#[test]
fn socket_roles_reject_unavailable_or_invalid_configuration() {
    // R4 no hand feature -> fail closed with feature diagnostic; R5 hand build
    // plus missing/wrong/relative arguments -> parser rejection before binding.
    for role in ["channel-listen", "channel-connect"] {
        for args in [
            vec![role],
            vec![role, "--unix", "relative"],
            vec![role, "--tcp", "/tmp/socket"],
        ] {
            let (ok, text) = run(&args);
            assert!(!ok);
            #[cfg(not(feature = "hand"))]
            assert!(text.contains("--features hand"));
            #[cfg(all(unix, feature = "hand"))]
            assert!(text.contains("--unix"), "{text}");
        }
    }
}

#[test]
fn scoped_exec_role_fails_closed_on_invalid_or_unsupported_requests() {
    // B20 Linux malformed args => usage; valid syntax outside the required
    // namespace => redacted InvalidInput, without a workload or private socket;
    // B21 non-Linux => explicit unsupported role, never an unscoped fallback.
    // Valid Linux execution belongs to the mandatory PID-namespace qualification.
    for args in [
        vec!["scoped-exec"],
        vec!["scoped-exec", "socket", "invalid-pid"],
    ] {
        let (ok, text) = run(&args);
        assert!(!ok);
        #[cfg(target_os = "linux")]
        assert!(
            if args.len() == 1 {
                text.contains("scoped-exec failed: InvalidInput")
            } else {
                text.contains("usage: awaken-sandbox scoped-exec")
            },
            "B20: {text}"
        );
        #[cfg(not(target_os = "linux"))]
        assert!(
            text.contains("requires a Linux PID namespace"),
            "B21: {text}"
        );
    }
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
            && text.contains("repository-publish-noreplace|read-tree-nofollow")
            && !text.contains("memoryd"),
        "with no role the binary prints usage: {text}"
    );
}

#[test]
fn repository_effect_role_rejects_invalid_input_without_echoing_payload() {
    // D3 Linux empty request -> bounded codec failure; unexpected argv -> usage.
    // D4 non-Linux -> unsupported internal role, never a host Git fallback.
    // Command::output closes stdin; a missing EOF would hang this entrypoint.
    for args in [
        vec!["repository-effect"],
        vec!["repository-effect", "synthetic-private-argument"],
    ] {
        let (ok, text) = run(&args);
        assert!(!ok);
        assert!(!text.contains("synthetic-private-argument"));
        if cfg!(target_os = "linux") && args.len() == 1 {
            assert!(
                text.contains("repository-effect transport failed"),
                "{text}"
            );
        } else {
            assert!(
                text.contains("repository-effect (Linux internal role)"),
                "{text}"
            );
        }
    }
}

#[test]
fn tree_reader_projects_an_absent_root_as_an_empty_archive() {
    /* Tree-reader decision table. Causes: C1 root is absent; C2 root is an
     * exact directory; C3 root is a non-directory. Effects: E1 emit a valid
     * empty archive; E2 emit the exact tree; E3 fail closed. This covers rule
     * T1 C1=>E1. `read-tree-nofollow --check` owns T2 C2=>E2 plus the symlink
     * rejection branch; T3 C3=>E3 is covered below. */
    let absent = std::env::temp_dir().join(format!(
        "awaken-absent-tree-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("unnamed")
    ));
    let output = awaken_sandbox()
        .args(["read-tree-nofollow", absent.to_str().unwrap()])
        .output()
        .expect("run absent tree reader");
    assert!(output.status.success(), "T1/E1: {:?}", output.stderr);
    let entries = tar::Archive::new(std::io::Cursor::new(output.stdout))
        .entries()
        .expect("T1/E1 valid empty tar")
        .count();
    assert_eq!(entries, 0, "T1/E1");
}

#[test]
fn tree_reader_rejects_a_non_directory_root() {
    /* Tree-reader decision-table rule T3: C3 a present non-directory root =>
     * E3 non-zero without treating its bytes as a directory projection. */
    let root = tempfile::tempdir().expect("temporary tree-reader root");
    let file = root.path().join("value");
    std::fs::write(&file, b"not a directory").expect("write fixture");
    let (ok, text) = run(&["read-tree-nofollow", file.to_str().unwrap()]);
    assert!(!ok, "T3/E3");
    assert!(text.contains("not one nofollow directory"), "T3/E3: {text}");
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
