"""Dependency boundary owned by the SandboxExecutionPolicy adapter."""

SANDBOX_POLICY_ALLOWED_DEPS = {
    "awaken-sandbox-policy-store": {
        "awaken-provisioning-contract",
        # This adapter owns one portable scoped schema bundle. The foundation
        # runners are infrastructure leaves; they do not pull server/runtime
        # vocabulary across the boundary.
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        "async-trait",
        "serde_json",
        "rusqlite",
        "sqlx",
        "tokio",
        "tempfile",
    }
}
