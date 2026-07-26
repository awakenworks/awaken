"""Dependency boundary owned by the SandboxExecutionPolicy adapter."""

SANDBOX_POLICY_ALLOWED_DEPS = {
    "awaken-sandbox-policy-store": {
        "awaken-provisioning-contract",
        "async-trait",
        "serde_json",
        "rusqlite",
        "sqlx",
        "tokio",
        "tempfile",
    }
}
