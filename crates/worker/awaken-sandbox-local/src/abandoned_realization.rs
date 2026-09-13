//! Where a cancelled provider creation leaves the name of the root it touched.
//!
//! The durable record for a filesystem-tier realization is the on-disk marker
//! that [`crate::realization_marker::begin`] writes before the first mutation,
//! and `observe_adoption` is the scan that reads it back. That covers the
//! effect itself. What it cannot cover is the window *before* it: a caller
//! dropped between the claim and the marker write leaves either nothing at all
//! or a half-prepared root, and the two are indistinguishable without walking
//! the whole provider base.
//!
//! So the claim's retention names the exact root. The ledger turns "scan every
//! sandbox under `base` and decide" into "look at this one", which is the
//! difference between a reconciliation an operator runs and one they don't.
//! It is deliberately in-process: it is an index into the durable evidence,
//! never a substitute for it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Roots whose realization was claimed and then dropped before it settled.
#[derive(Default)]
pub struct AbandonedRealizations {
    roots: Mutex<Vec<PathBuf>>,
}

impl AbandonedRealizations {
    /// The retention handed to [`pc::EffectClaim::durable`](awaken_provisioning_contract::EffectClaim::durable):
    /// it records `root` if and only if the claimed creation is dropped
    /// unsettled. Built before the effect, because a retention constructed
    /// afterwards would be exactly the ordering this whole mechanism exists to
    /// forbid.
    pub(crate) fn retain(self: &Arc<Self>, root: PathBuf) -> impl FnOnce() + Send + 'static {
        let ledger = Arc::clone(self);
        move || {
            ledger
                .roots
                .lock()
                .expect("abandoned realization ledger poisoned")
                .push(root);
        }
    }

    /// Every root recorded so far, oldest first.
    #[must_use]
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .lock()
            .expect("abandoned realization ledger poisoned")
            .clone()
    }

    /// Whether `root` is among them.
    #[must_use]
    pub fn contains(&self, root: &Path) -> bool {
        self.roots
            .lock()
            .expect("abandoned realization ledger poisoned")
            .iter()
            .any(|recorded| recorded == root)
    }
}

/// The scan a filesystem-tier claim names: the reconciler that reads a
/// provider realization marker back and decides the stranded root's fate.
/// Named once here so the two providers cannot disagree about who reconciles
/// them, and so the name stays a real symbol rather than a string.
pub(crate) const REALIZATION_RECONCILER: &str = "observe_adoption";
