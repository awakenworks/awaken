"""Dependency allowlists for the ADR-0071 registration boundary."""

EXECUTABLE_AGENT_ALLOWED_DEPS: dict[str, set[str]] = {
    # The command composes the existing immutable snapshot and Session view;
    # implementations, stores, and HTTP clients remain outside this port crate.
    "awaken-executable-agent-contract": {
        "awaken-runtime-contract",
        "awaken-session-contract",
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
        "awaken-runtime-contract",
        "awaken-session-contract",
        "awaken-resource-contract",
        "async-trait",
        "tokio",
    },
}
