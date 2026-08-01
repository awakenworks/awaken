"""Dependency allowlists for shared static-resource contracts and applications."""

DOMAIN_APPLICATION_ALLOWED_DEPS: dict[str, set[str]] = {
    # Control-owned static Environment aggregate and repository port. It remains
    # an inward contract leaf; executable projection and WorkQueue stay outward.
    "awaken-environment-contract": {
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
    },
    # Coordinator-neutral image realization aggregate and ports. It consumes
    # immutable Environment facts and names no persistence or builder adapter.
    "awaken-environment-realization-contract": {
        "awaken-environment-contract",
        "async-trait",
        "serde",
        "thiserror",
    },
    # Coordinator application/state machine for durable image-build demand.
    "awaken-environment-image-build": {
        "awaken-environment-contract",
        "awaken-environment-realization-contract",
        "async-trait",
        "tokio",
        "awaken-executable-environment-catalog",
    },
    # Control-owned Environment application command path. Protocol and Admin
    # adapters translate into this service rather than coordinating stores.
    "awaken-environment-application": {
        "awaken-admin-assistant",
        "awaken-environment-contract",
        "awaken-env-store",
        "awaken-executable-environment-contract",
        "awaken-provisioning-contract",
        "async-trait",
        "thiserror",
        "tokio",
    },
    # Resources application services own cross-repository command ordering. HTTP
    # and Runtime consume only their inward ports from awaken-resource-contract.
    "awaken-file-application": {
        "awaken-resource-contract",
        "awaken-file-store",
        "awaken-resource-store",
        "async-trait",
        "chrono",
        "tokio",
        "uuid",
    },
    "awaken-resource-application": {
        "awaken-resource-contract",
        "awaken-file-application",
        "awaken-file-store",
        "awaken-memory-store",
        "awaken-resource-store",
        "awaken-skill-store",
        "async-trait",
        "tokio",
    },
}
