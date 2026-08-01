"""Dependency allowlists for the ADR-0071 registration boundary."""

EXECUTABLE_AGENT_ALLOWED_DEPS: dict[str, set[str]] = {
    # One transport-parsed private Bearer comparison shared by independent
    # Control-to-Coordinator adapters. It owns no domain or wire dependency.
    "awaken-service-auth-contract": set(),
    "awaken-executable-environment-contract": {
        "awaken-environment-contract",
        "awaken-provisioning-contract",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
    },
    "awaken-executable-environment-catalog": {
        "awaken-executable-environment-contract",
        "awaken-environment-contract",
        "awaken-provisioning-contract",
        "awaken-scoped-migration",
        "awaken-service-auth-contract",
        "async-trait",
        "axum",
        "reqwest",
        "serde",
        "serde_json",
        "sqlx",
        "tokio",
    },
    # The command composes the immutable runtime snapshot with the exact
    # Session-facing publication profile. Implementations, stores, and HTTP clients
    # remain outside this port crate.
    "awaken-executable-agent-contract": {
        "awaken-agent-contract",
        "awaken-resource-contract",
        "awaken-runtime-contract",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
    },
    # Coordinator-owned rebuildable projection plus the explicit AllInOne local
    # registrar. Network/store adapters extend this owner; Config Service never
    # implements a second catalog.
    "awaken-executable-agent-catalog": {
        "awaken-executable-agent-contract",
        "awaken-service-auth-contract",
        "awaken-runtime-contract",
        "awaken-scoped-migration",
        "awaken-resource-contract",
        "async-trait",
        "axum",
        "reqwest",
        "serde",
        "serde_json",
        "sqlx",
        "tokio",
    },
}
