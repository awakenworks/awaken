//! Architecture fitness function: boundary values are plain serializable data.
//!
//! INVARIANTS G3 (config edge is serializable data: `ResolvedSpec` + fingerprint;
//! no live registry, `Arc<dyn ...>`, pin, scope, or handle crosses) and G20
//! (executor/wait-resume channels are data-only).
//!
//! `Serialize + DeserializeOwned` IS the "plain data, no handles" guarantee: a
//! trait object, channel, or process handle does not implement `DeserializeOwned`,
//! so the moment one is added to a boundary type this stops compiling. Enforced
//! for free by `cargo check --workspace --all-targets` (pre-commit, `rust`) and
//! `cargo test --workspace` (pre-push); no separate hook is needed.
//!
//! When durable live-command delivery and wait/resume exec request/result
//! channels land (G20), add their value types here — that is the point at which
//! serde becomes a hard requirement for them.

use serde::Serialize;
use serde::de::DeserializeOwned;

fn assert_boundary<T: Serialize + DeserializeOwned>() {}

#[test]
fn boundary_values_are_plain_serializable_data() {
    use awaken_runtime_contract as rc;

    // G3: the config -> runtime edge.
    assert_boundary::<rc::resolved::ResolvedSpec>();
    assert_boundary::<rc::resolved::CatalogFingerprint>();
    assert_boundary::<rc::resolved::ModelBinding>();
    assert_boundary::<rc::resolved::ToolDescriptor>();
    assert_boundary::<rc::resolved::ResolvedRun>();

    // Run activation crosses adapter -> ingress -> runtime as data.
    assert_boundary::<rc::activation::RunActivation>();

    // G29: the complete catalog install request handed to the runtime.
    assert_boundary::<rc::catalog::RuntimeCatalogInstall>();
}
