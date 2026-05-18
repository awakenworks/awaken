//! Operator-tunable caps for the `/v1/eval/*` endpoints. Lifted out of
//! `app.rs` so adding/changing a knob doesn't drag the central server
//! config file past its line cap.

use serde::{Deserialize, Serialize};

/// Per-cap defaults match the values these constants used to hold in
/// `services::eval_run_service` / `online_eval_service` / `dataset_service`
/// — operators only override when a deployment has different appetites.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EvalLimits {
    /// Soft cap on TOTAL replay units (`fixtures × matrix × samples`)
    /// per synchronous `POST /v1/eval/runs`. Above this the endpoint
    /// returns 400 with a "split or persist" hint so a long-running
    /// matrix doesn't hold the HTTP connection past ingress timeouts.
    pub max_cells_per_sync_run: usize,
    /// Per-cell concurrency cap in dataset matrix runs. Bounds the
    /// burst put on rate-limited upstream providers.
    pub max_concurrent_matrix_cells: usize,
    /// Soft cap on `cells × samples` for `POST /v1/eval/online`.
    pub max_cells_per_sync_online: usize,
    /// Per-cell concurrency cap in online eval (same role as the
    /// dataset matrix cap, separate knob to tune the ad-hoc path).
    pub max_concurrent_online_cells: usize,
    /// Hard cap on per-cell flakiness sample count (`samples=N` in
    /// the request body). Honoured by both `/v1/eval/runs` and
    /// `/v1/eval/online`.
    pub max_samples_per_cell: u32,
    /// Hard ceiling on revise-on-judge-fail iterations. Three rewrites
    /// is usually plenty; more typically means the rubric is broken.
    pub max_judge_revisions: u32,
    /// Default max_count cap for `POST /v1/eval/datasets/:id/import-traces`
    /// when the request body omits it.
    pub default_import_traces_max: usize,
}

impl Default for EvalLimits {
    fn default() -> Self {
        Self {
            max_cells_per_sync_run: 100,
            max_concurrent_matrix_cells: 5,
            max_cells_per_sync_online: 10,
            max_concurrent_online_cells: 5,
            max_samples_per_cell: 20,
            max_judge_revisions: 3,
            default_import_traces_max: 50,
        }
    }
}

impl EvalLimits {
    /// Reject configurations that would hang every request: a
    /// `Semaphore::new(0)` makes `acquire_owned()` block forever, so
    /// the first eval task posted to the server would never return.
    /// Caller (server startup) propagates this as a fatal load error
    /// instead of letting the box come up in a wedged state.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_concurrent_matrix_cells == 0 {
            return Err("eval_limits.max_concurrent_matrix_cells must be > 0".into());
        }
        if self.max_concurrent_online_cells == 0 {
            return Err("eval_limits.max_concurrent_online_cells must be > 0".into());
        }
        Ok(())
    }
}

/// Boot-time guard: maps [`EvalLimits::validate`] into `std::io::Error`
/// so the server's startup chain (which already returns `io::Result`)
/// can refuse to come up without growing a fresh error type.
pub fn validate_eval_limits(limits: &EvalLimits) -> std::io::Result<()> {
    limits
        .validate()
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidInput, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_zero_matrix_concurrency() {
        let lim = EvalLimits {
            max_concurrent_matrix_cells: 0,
            ..EvalLimits::default()
        };
        let err = lim.validate().unwrap_err();
        assert!(err.contains("max_concurrent_matrix_cells"), "{err}");
    }

    #[test]
    fn validate_rejects_zero_online_concurrency() {
        let lim = EvalLimits {
            max_concurrent_online_cells: 0,
            ..EvalLimits::default()
        };
        let err = lim.validate().unwrap_err();
        assert!(err.contains("max_concurrent_online_cells"), "{err}");
    }

    #[test]
    fn validate_accepts_defaults() {
        assert!(EvalLimits::default().validate().is_ok());
    }
}
