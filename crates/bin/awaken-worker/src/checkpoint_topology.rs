// @formal surface exclude=composition_wiring
// @formal owner crates/bin/awaken-worker/src/lib.rs
// @formal reason: Construction-time topology check split out of WorkerNodeBuilder::build. It reads
// @formal reason: provider capabilities once, before any Environment exists, and owns no durable state.
//! Checkpoint topology admitted at Worker construction.
//!
//! | Rule | provider formats | checkpoint store | restore rung | Effect |
//! |---|---|---|---|---|
//! | K1 | none | absent | any | ordinary mode |
//! | K2 | some | absent | any | refused |
//! | K3 | none | present | any | refused |
//! | K4 | some | present | Unsupported | refused before any Environment exists |
//! | K5 | some | present | targets or restorable | admitted |

use std::collections::BTreeSet;
use std::sync::Arc;

use awaken_provisioning_contract::{SandboxProvider, SessionSandbox};

use super::WorkerNodeBuildError;

/// Refuse a checkpoint composition that cannot restore what it advertises.
/// Advertised formats promise restore, so the restore rung is consulted here
/// instead of being discovered mid-run.
pub(super) fn validate(
    provider: Option<&Arc<dyn SandboxProvider<Environment = dyn SessionSandbox>>>,
    formats: &BTreeSet<String>,
    checkpoint_store: bool,
) -> Result<(), WorkerNodeBuildError> {
    let refusal = match (formats.is_empty(), checkpoint_store) {
        (true, false) => return Ok(()),
        (false, false) => "checkpoint-capable Session provider requires a checkpoint store",
        (true, true) => "checkpoint store requires a checkpoint-capable Session provider",
        (false, true)
            if provider.is_some_and(|provider| provider.restoration().targets().is_err()) =>
        {
            "checkpoint-capable Session provider requires a restore rung"
        }
        (false, true) => return Ok(()),
    };
    Err(WorkerNodeBuildError(refusal.to_string()))
}
