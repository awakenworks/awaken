//! The shared readiness mapping for a role's cloud-native admin surface, so the
//! Brain (serve) and the worker return `/readyz` identically. The metrics `/metrics`
//! surface is NOT hand-formatted here — each role exposes OpenTelemetry observable
//! gauges through the OSS Prometheus exporter (`awaken_observability::AdminMeter`).

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
}
