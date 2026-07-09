//! ADR-0048 assembly validation: prove `awaken-iam-host` (the single-PDP `IamGate`
//! + `auth_layer` PEP) works against our pinned iam rev, ahead of adopting it in
//! place of the hand-rolled `ManagementAuthz` / `EnforceEngine`.
//!
//! What this locks:
//!   1. **Zero-config Local embed** — `HostConfig::local_in_memory()` boots with no
//!      dir, no seal key, no env, and mints an ephemeral bootstrap admin token that
//!      authenticates (the single-machine "log in with the admin token" story).
//!   2. **Fail-closed authn** — no credential and a bogus credential both fail to
//!      resolve a principal; only the real ephemeral admin authenticates.
//!
//! Caveat for the actual adoption: `auth_layer(gate, actions)` (the tower PEP)
//! does not `.layer()` directly onto our `axum::Router` — the `IamAuthService`
//! bounds want the inner service's exact `Response`/`Error`, so the HTTP wiring
//! will go through an `axum::middleware::from_fn` adapter around the gate rather
//! than the raw layer. That wiring lands with the real engine replacement; here we
//! validate the assembly's authn/authz core against our pinned rev.

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
