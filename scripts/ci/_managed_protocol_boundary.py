"""Dependency boundary owned by the Managed Agents anti-corruption adapter."""

MANAGED_PROTOCOL_ALLOWED_DEPS = {
    # Managed Deployment persistence port. The protocol adapter owns wire/domain
    # projection; this leaf carries only opaque records, occurrence claims, and
    # the shared lifecycle fact value needed for transactional outbox commits.
    "awaken-deployment-contract": {
        "awaken-session-contract",
        "async-trait",
        "chrono",
        "chrono-tz",
        "thiserror",
    },
    "awaken-protocol-managed": {
        "awaken-agent-contract",
        "awaken-executable-agent-contract",
        "awaken-executable-environment-contract",
        "awaken-executable-environment-catalog",
        "awaken-environment-contract",
        "awaken-environment-realization-contract",
        "awaken-environment-application",
        "awaken-credential-contract",
        "awaken-session-contract",
        "awaken-deployment-contract",
        "awaken-dream-application",
        "awaken-resource-contract",
        # Dev-only Session/Resource conformance uses the authoritative Resources
        # catalog adapter rather than reconstructing it in Admin.
        "awaken-resource-store",
        "awaken-work-store",
        "awaken-env-store",
        "awaken-session-store",
        "async-stream",
        "form_urlencoded",
        # Wire timestamp parsing/projection; Deployment scheduling itself lives
        # in awaken-deployment-contract's Cron value object.
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
