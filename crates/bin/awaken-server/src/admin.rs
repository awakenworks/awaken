//! Shared building blocks for a role's cloud-native admin surface (readiness +
//! Prometheus metrics), so the Brain (serve) and the worker format their probes and
//! gauges identically instead of each hand-rolling the strings. Each role keeps its
//! own router and its own signals (the Brain scales on `active_streams`, the worker
//! on whether its dispatch pool is claiming); only the response/format boilerplate is
//! shared here.

use axum::http::StatusCode;

/// The readiness response: `200 ready` while serving, `503 draining` once the role is
/// draining for scale-in — the single mapping every role's `/readyz` returns.
#[must_use]
pub fn readyz(ready: bool) -> (StatusCode, &'static str) {
    if ready {
        (StatusCode::OK, "ready\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "draining\n")
    }
}

/// One Prometheus gauge in text exposition format (HELP + TYPE + value), so every
/// role's `/metrics` emits well-formed, consistent gauges. Concatenate several for a
/// multi-gauge endpoint.
#[must_use]
pub fn prometheus_gauge(name: &str, help: &str, value: u64) -> String {
    format!(
        "# HELP {name} {help}\n\
         # TYPE {name} gauge\n\
         {name} {value}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readyz_maps_readiness_to_200_or_503() {
        assert_eq!(readyz(true).0, StatusCode::OK);
        assert_eq!(readyz(true).1, "ready\n");
        assert_eq!(readyz(false).0, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(readyz(false).1, "draining\n");
    }

    #[test]
    fn gauge_is_well_formed_prometheus_text() {
        let g = prometheus_gauge("awaken_worker_draining", "1 when draining.", 1);
        assert!(g.contains("# HELP awaken_worker_draining 1 when draining."));
        assert!(g.contains("# TYPE awaken_worker_draining gauge"));
        assert!(g.contains("awaken_worker_draining 1\n"));
    }
}
