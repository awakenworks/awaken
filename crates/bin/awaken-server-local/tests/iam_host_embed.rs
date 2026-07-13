//! ADR-0048 assembly validation: prove `awaken-iam-host` (the single-PDP `IamGate`
//! + `auth_layer` PEP) works against our pinned iam rev.
//!
//! What this locks:
//!   1. **Zero-config Local embed** — `HostConfig::local_in_memory()` boots with no
//!      dir, no seal key, no env, and mints an ephemeral bootstrap admin token that
//!      authenticates (the single-machine "log in with the admin token" story).
//!   2. **Fail-closed authn** — no credential and a bogus credential both fail to
//!      resolve a principal; only the real ephemeral admin authenticates.
//!
//! **Adoption outcome (ADR-0048): engine shared; PEP + embed stay local.** The
//! authn/authz *core* validated here is sound and the *engine* is already shared
//! (`authz.rs` composes iam-core/-server/-preset directly). Two of the original
//! blockers were *fixed by extending host* at rev `fe7eb6c`:
//! `RouteActions::scope_for` removed the hard-coded `ScopeRef::Global` (so the PEP
//! can now enforce per-workspace tenancy), and `IamGate::from_local_authz` lets a
//! product wrap its own embed (identity + grants) instead of `embed_local`'s fixed
//! grant-less default. The swap still stays out of this plane because it remains a
//! net regression for the Managed surface — `x-api-key` auth, the Managed
//! `ErrorResponse` envelope, token-home-workspace authz for workspace-less routes,
//! and the single-lock mint invariant (see the `authz.rs` module doc, "iam-host
//! (ADR-0048)"). This test keeps host's assembly regression-checked against our
//! rev so a future consumer that fits the PEP can adopt it.

use awaken_iam_contract::{PrincipalRef, Timestamp};
use awaken_iam_host::{HostConfig, embed_local};

const NOW_UNIX: i64 = 1_751_328_000; // 2025-07-01 UTC — the bootstrap token never expires.

fn now_ts() -> Timestamp {
    Timestamp("2026-01-01T00:00:00Z".into())
}

#[test]
fn zero_config_local_embed_mints_an_ephemeral_admin_that_authenticates() {
    // No dir, no seal key, no env: the single-machine zero-config path.
    let handle = embed_local(&HostConfig::local_in_memory()).expect("zero-config embed");

    assert!(
        handle.admin_token.starts_with("sk-awaken-"),
        "bootstrap admin token is Awaken-branded; got {}",
        &handle.admin_token[..16.min(handle.admin_token.len())]
    );

    let principal = handle
        .gate
        .authenticate_bearer(&handle.admin_token, &now_ts(), NOW_UNIX)
        .expect("the ephemeral admin token authenticates");
    assert!(
        matches!(principal, PrincipalRef::Service { .. }),
        "admin token resolves to a service principal"
    );
}

#[test]
fn embedded_gate_is_fail_closed_and_only_the_admin_authenticates() {
    let handle = embed_local(&HostConfig::local_in_memory()).expect("zero-config embed");
    let ts = now_ts();

    // A bogus, well-formed-looking credential resolves to no principal.
    assert!(
        handle
            .gate
            .authenticate_bearer("sk-awaken-nope.definitely-not-real", &ts, NOW_UNIX)
            .is_none(),
        "a forged token must not authenticate"
    );
    // Empty / garbage input likewise fails closed.
    assert!(
        handle.gate.authenticate_bearer("", &ts, NOW_UNIX).is_none(),
        "an empty credential must not authenticate"
    );
    // Only the real ephemeral admin authenticates.
    assert!(
        handle
            .gate
            .authenticate_bearer(&handle.admin_token, &ts, NOW_UNIX)
            .is_some(),
        "the bootstrap admin authenticates"
    );
}
