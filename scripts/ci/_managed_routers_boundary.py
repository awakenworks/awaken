"""Dependency boundary owned by the Managed HTTP adapter collection."""

MANAGED_ROUTERS_ALLOWED_DEPS = {
    # The per-resource HTTP adapters drive public application APIs while keeping
    # wire parsing and response projection outside the runtime substrate.
    "awaken-managed-routers": {
        "awaken-runtime-host",
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-resource-contract",
        "awaken-memory-store",
        "awaken-skill-store",
        "awaken-ext-skills",
        "awaken-data-subject",
        "awaken-protocol-transport",
        # dev-only: Art.17 fan-out test erases harvested ACP session homes.
        "awaken-run-executor-acp",
        # File ownership is selected by the same neutral WorkspaceScope used by
        # every other platform resource adapter.
        "awaken-tenancy",
        "async-trait",
        "axum",
        "base64",
        "serde",
        "serde_json",
        "sha2",
        "tempfile",
        "tokio",
        "tower",
        "uuid",
    },
}
