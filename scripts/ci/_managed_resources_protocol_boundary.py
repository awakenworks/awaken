"""Dependency boundary owned by the Anthropic-compatible Resources protocol leaf."""

MANAGED_RESOURCES_PROTOCOL_ALLOWED_DEPS = {
    # The per-resource HTTP adapters drive public application APIs while keeping
    # wire parsing and response projection outside the runtime substrate.
    "awaken-protocol-managed-resources": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-resource-contract",
        "awaken-memory-store",
        "awaken-skill-store",
        "awaken-ext-skills",
        "awaken-session-contract",
        # File ownership is selected by the same neutral WorkspaceScope used by
        # every other platform resource adapter.
        "awaken-tenancy",
        "axum",
        "serde",
        "serde_json",
        "tokio",
        "tower",
        "zip",
    },
}
