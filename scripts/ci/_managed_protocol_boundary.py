"""Dependency boundary owned by the Managed Agents anti-corruption adapter."""

MANAGED_PROTOCOL_ALLOWED_DEPS = {
    "awaken-protocol-managed": {
        "awaken-agent-contract",
        "awaken-credential-contract",
        "awaken-session-contract",
        "awaken-ext-memory",
        "awaken-resource-contract",
        "awaken-work-store",
        "awaken-env-store",
        "awaken-session-store",
        "async-stream",
        "form_urlencoded",
        # Public Deployment cron semantics are product-wire behavior. These are
        # pure calendar libraries, not a host/provider/runtime dependency; moving
        # the scheduler into a neutral crate would invert domain ownership.
        "chrono",
        "chrono-tz",
        "awaken-tenancy",
        "awaken-credential-vault",
        "awaken-managed-bridge",
        "awaken-provisioning-contract",
        "awaken-sandbox-policy-store",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        "axum",
        "tokio-stream",
        "tracing",
        "tower",
        "http-body-util",
        # Dev-only verification dependencies are still explicit here; the
        # manifest-section policy independently prevents them moving to prod.
        "awaken-config-resolver",
        "awaken-model-catalog",
    }
}
