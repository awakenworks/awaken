"""Dependency boundary owned by concrete sandbox providers."""

SANDBOX_PROVIDER_ALLOWED_DEPS = {
    # Container/K8s provider: realizes neutral sandbox ports over a
    # dependency-inverted ContainerRuntime port and pure plan renderers.
    "awaken-sandbox-container": {
        "awaken-provisioning-contract",
        # Dev-only Session proof drives ToolExecutor through the real hand wire.
        "awaken-runtime-contract",
        "awaken-tool-relay",
        # The ACP bridge receives an AgentChannel rather than provider details.
        "awaken-agent-channel",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        # Resolve bytes through BlobSource and verify declared content hashes.
        "blake3",
        # Optional connection, Docker, and Kubernetes adapters.
        "awaken-connection",
        "tokio",
        "tar",
        "bollard",
        "base64",
        "futures-util",
        "kube",
        "k8s-openapi",
        "rustls",
        # Dev-only package/image coordinator persistence fixtures.
        "tempfile",
    }
}
