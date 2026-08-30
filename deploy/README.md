# Deploy Awaken

This document is the deployment and operator entry point. Local evaluation is
owned by the repository [quickstart](../README.md#try-awaken); k3d scenarios are
distributed verification fixtures rather than a second installation path.

## Process modes

```console
awaken all-in-one --no-browser       # Control + Coordinator + local Worker
awaken control --config /etc/awaken/control.toml
awaken coordinator --config /etc/awaken/coordinator.toml
awaken-worker --config /etc/awaken/worker.toml
awaken database migrate --config /etc/awaken/control.toml
awaken config --json                 # redacted effective configuration
```

`awaken` owns AllInOne, Control, and Coordinator composition. Execution-only
Worker deployment uses the separate `awaken-worker` binary; there is no
overlapping `awaken worker` compatibility command.

The root Compose quickstart intentionally selects `sandbox_tier = "local"` for
single-user evaluation, with the non-root Management container as the outer
boundary. Shared or multi-tenant deployments must select an isolated Namespace
or Kubernetes sandbox and provide its required runtime policy instead of
reusing that evaluation preset.

Production configuration comes only from an explicit `--config` path or the
standard `~/.awaken/config.toml`, followed by typed defaults. `AWAKEN_*`
environment variables are not deployment, model, business, or credential
configuration sources.

## Local and shared storage

All embedded databases and the generated `control-seal.key` live under the one
data directory. Local mode migrates embedded stores at startup. Shared server
deployments run `awaken database migrate` before application Pods; application
startup then verifies the existing schema without writing DDL. Shared
deployments select Postgres with `runtime_database_url`,
`resource_database_url`, and the per-component Control database fields shown by
`awaken config`; database URLs and key material are always redacted.

The typed configuration file can hold bootstrap settings:

```toml
data_dir = "/srv/awaken"
bind = "127.0.0.1:8080"
mode = "local"
run_local_pool = true
no_browser = false
```

The file also accepts `runtime_database_url`, `resource_database_url`, and
`catalog_db` / `credential_db` / `config_db` / `admin_db` / `data_subject_db` /
`environment_db` / `sessions_db` / `captured_content_db` for server deployments.
A deployment using one PostgreSQL authority for the complete AllInOne management
plane may instead set `management_database_url_file` to an operator-projected
secret file. It cannot be mixed with the per-store URL fields. Seal keys and
Cloud workload credentials likewise use their existing file-backed settings.

## Split Worker

A split Worker has database-free execution configuration. Credential material
is projected into its trust domain by Kubernetes Secret, Vault CSI, or an
equivalent secret-volume provider:

```toml
role = "worker"
mode = "server"
worker_server = "https://awaken-coordinator:8443"
worker_server_ca_certificate_file = "/run/awaken/worker-server-ca.pem"
worker_request_credential_file = "/run/awaken/worker-request-credential.json"
worker_credential_material_root = "/run/awaken/credentials"
worker_credential_trust_domain = "awaken.worker"
```

The Worker rejects Control/Coordinator/Resource database URLs, Control seal-key
settings, and private Control-to-Coordinator tokens. Registration installs the
claim-fenced File, Memory, Skill, and Repository-verification clients; advertised
capabilities derive from those installed adapters and fail closed when required
adapters are absent.

## Split service boundaries

Split Control and Coordinator processes each bind a second private HTTP surface.
`bind` serves public/product routes; `internal_bind` serves authenticated service
routes. Expose each internal port through a separate ClusterIP Service without
Ingress and restrict callers with NetworkPolicy.

```toml
# Control configuration
role = "control"
bind = "0.0.0.0:8080"
internal_bind = "0.0.0.0:8081"
coordinator_internal_url = "http://awaken-coordinator-private:8081"
executable_agent_registration_token_file = "/var/run/secrets/awaken/agent-registration-token"

# Coordinator configuration; keep this in a separate file.
role = "coordinator"
bind = "0.0.0.0:8080"
internal_bind = "0.0.0.0:8081"
control_internal_url = "http://awaken-control-private:8081"
control_service_token_file = "/var/run/secrets/awaken/control-service-token"
executable_agent_registration_token_file = "/var/run/secrets/awaken/agent-registration-token"
```

Split roles fail startup when `internal_bind` is absent, malformed, or equal to
`bind`; AllInOne and Worker reject it. Token values are loaded from files and
never appear in `awaken config`. `awaken database migrate` also prepares the
Coordinator-owned executable Agent command log when `runtime_database_url` is
configured.
