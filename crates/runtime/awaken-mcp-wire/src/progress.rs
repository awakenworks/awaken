//! MCP progress reporting types and helpers.
//!
//! A server that received a `progressToken` in a call's `_meta` may emit
//! `notifications/progress` while the call runs. The transport routes those to
//! the per-call channel; these helpers let a consumer normalize and throttle
//! them for display.

use std::time::{Duration, Instant};

/// Progress update from an MCP tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct McpProgressUpdate {
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

/// Minimum interval between progress emissions.
pub const MCP_PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Minimum delta in normalized progress before re-emitting.
pub const MCP_PROGRESS_MIN_DELTA: f64 = 0.01;

/// Gate that throttles progress emissions.
#[derive(Default)]
pub struct ProgressEmitGate {
    last_emit_at: Option<Instant>,
    last_progress: Option<f64>,
    last_message: Option<String>,
}

/// Normalize a progress update to the [0.0, 1.0] range.
pub fn normalize_progress(update: &McpProgressUpdate) -> Option<f64> {
    if !update.progress.is_finite() {
        return None;
    }
    match update.total {
        Some(total) if total.is_finite() && total > 0.0 => {
            Some((update.progress / total).clamp(0.0, 1.0))
        }
        _ => Some(update.progress),
    }
}

/// Whether progress should be emitted, given the throttling gate.
pub fn should_emit_progress(
    gate: &mut ProgressEmitGate,
    progress: f64,
    message: Option<&str>,
) -> bool {
    let now = Instant::now();
    let interval_elapsed = gate
        .last_emit_at
        .is_none_or(|last| now.duration_since(last) >= MCP_PROGRESS_MIN_INTERVAL);
    let delta_large_enough = gate
        .last_progress
        .is_none_or(|last| (progress - last).abs() >= MCP_PROGRESS_MIN_DELTA);
    let message_changed = message != gate.last_message.as_deref();
    let terminal = progress >= 1.0;

    if !(interval_elapsed || delta_large_enough || message_changed || terminal) {
        return false;
    }

    gate.last_emit_at = Some(now);
    gate.last_progress = Some(progress);
    gate.last_message = message.map(ToOwned::to_owned);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_with_total() {
        let update = McpProgressUpdate {
            progress: 3.0,
            total: Some(10.0),
            message: None,
        };
        assert!((normalize_progress(&update).unwrap() - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn normalize_without_total() {
        let update = McpProgressUpdate {
            progress: 0.5,
            total: None,
            message: None,
        };
        assert!((normalize_progress(&update).unwrap() - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn normalize_clamps_above_1() {
        let update = McpProgressUpdate {
            progress: 20.0,
            total: Some(10.0),
            message: None,
        };
        assert!((normalize_progress(&update).unwrap() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn normalize_non_finite_returns_none() {
        let update = McpProgressUpdate {
            progress: f64::NAN,
            total: Some(10.0),
            message: None,
        };
        assert!(normalize_progress(&update).is_none());
    }

    #[test]
    fn normalize_zero_total_uses_raw() {
        let update = McpProgressUpdate {
            progress: 0.7,
            total: Some(0.0),
            message: None,
        };
        assert!((normalize_progress(&update).unwrap() - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn normalize_clamps_below_zero_with_total() {
        // A ratio below zero clamps up to the 0.0 floor (matches the upper clamp).
        let update = McpProgressUpdate {
            progress: -3.0,
            total: Some(10.0),
            message: None,
        };
        assert!((normalize_progress(&update).unwrap() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn normalize_negative_total_uses_raw() {
        // A non-positive total is not a valid denominator; fall back to the raw
        // progress rather than dividing by it.
        let update = McpProgressUpdate {
            progress: 0.4,
            total: Some(-5.0),
            message: None,
        };
        assert!((normalize_progress(&update).unwrap() - 0.4).abs() < f64::EPSILON);
    }

    #[test]
    fn normalize_non_finite_total_uses_raw() {
        // An infinite total is not usable; fall back to the raw progress.
        let update = McpProgressUpdate {
            progress: 0.6,
            total: Some(f64::INFINITY),
            message: None,
        };
        assert!((normalize_progress(&update).unwrap() - 0.6).abs() < f64::EPSILON);
    }

    #[test]
    fn gate_first_emission_always_passes() {
        let mut gate = ProgressEmitGate::default();
        assert!(should_emit_progress(&mut gate, 0.1, None));
    }

    #[test]
    fn gate_blocks_small_delta_within_interval() {
        let mut gate = ProgressEmitGate::default();
        assert!(should_emit_progress(&mut gate, 0.1, None));
        assert!(!should_emit_progress(&mut gate, 0.1001, None));
    }

    #[test]
    fn gate_allows_large_delta() {
        let mut gate = ProgressEmitGate::default();
        assert!(should_emit_progress(&mut gate, 0.1, None));
        assert!(should_emit_progress(&mut gate, 0.5, None));
    }

    #[test]
    fn gate_allows_message_change() {
        let mut gate = ProgressEmitGate::default();
        assert!(should_emit_progress(&mut gate, 0.1, Some("start")));
        assert!(should_emit_progress(&mut gate, 0.1, Some("end")));
    }

    #[test]
    fn gate_allows_terminal_progress() {
        let mut gate = ProgressEmitGate::default();
        assert!(should_emit_progress(&mut gate, 0.99, None));
        assert!(should_emit_progress(&mut gate, 1.0, None));
    }

    #[test]
    fn gate_allows_message_cleared_to_none() {
        // Clearing the message (Some -> None) is a change and must emit even when
        // the progress delta is below threshold within the interval.
        let mut gate = ProgressEmitGate::default();
        assert!(should_emit_progress(&mut gate, 0.1, Some("working")));
        assert!(should_emit_progress(&mut gate, 0.1001, None));
    }
}
