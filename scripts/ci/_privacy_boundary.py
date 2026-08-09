"""Dependency boundaries for Control consent and Coordinator captured content."""

PRIVACY_ALLOWED_DEPS = {
    # Control-owned Data Subject aggregate, consent, and erasure process manager.
    # Runtime-contract supplies only the neutral subject/capture/erasure ports.
    "awaken-data-subject": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "rusqlite",
        "tokio",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        "sqlx",
        "tempfile",
    },
    # Coordinator-owned subject-tagged content persistence. It implements only
    # neutral capture/erasure ports and owns an independent migration scope.
    "awaken-captured-content-store": {
        "awaken-runtime-contract",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        "async-trait",
        "rusqlite",
        "serde_json",
        "sqlx",
        "tempfile",
        "thiserror",
        "tokio",
    },
}
