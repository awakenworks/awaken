# Awaken Runtime

This repository hosts the `awaken-runtime` design corpus and guardrails. The
runtime protocol/specification and conformance surface are licensed under
Apache-2.0; code packages may use their own file or package license metadata.

Start with [docs/README.md](docs/README.md) for the bounded contexts, runtime
coverage target, architecture invariants, and documentation checks.

## Quick start

Build one program, then start it:

```console
cargo build -p awaken-cli --bin awaken
awaken all-in-one
```

Running `awaken` without a command is the same as `awaken all-in-one`: it starts the
API, embedded web console, and local worker, persists all local state under
`~/.awaken`, and opens `http://127.0.0.1:8080`. Use `--port`, `--data-dir`, or
`--no-browser` for common local overrides. The resulting binary contains the
complete console and needs no external web directory or Node.js at runtime.

Useful operational commands:

```console
awaken all-in-one --no-browser       # Control + Coordinator + local Worker
awaken control --config /etc/awaken/config.toml
awaken coordinator --config /etc/awaken/config.toml
awaken worker --config /etc/awaken/config.toml --server https://coordinator
awaken config                        # effective config and database paths
awaken config --json                 # redacted machine-readable report
awaken --version
```

Production configuration comes only from an explicit `--config` path or the
standard `~/.awaken/config.toml`, followed by typed defaults. `AWAKEN_*`
environment variables are not deployment, model, business, or credential
configuration sources.

All embedded databases and the generated `control-seal.key` live under the one
data directory. Local mode migrates embedded stores at startup. Shared server
deployments run `awaken database migrate` before application Pods; application
startup then verifies the existing schema without writing DDL. Shared
deployments select Postgres with `runtime_database_url`,
`resource_database_url`, and the per-component control database fields shown by
`awaken config`; database URLs and key material are always redacted.

The typed configuration file can hold bootstrap settings. For example:

```toml
data_dir = "/srv/awaken"
bind = "127.0.0.1:8080"
mode = "local"
run_local_pool = true
no_browser = false
```

A split Worker has a database-free execution configuration. Credential material
is projected into its own trust domain by Kubernetes Secret, Vault CSI, or an
equivalent secret-volume provider; Awaken reads only the exact publication pin:

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
settings, and private Control-to-Coordinator tokens. It does not require a
Control seal key. Registration installs claim-fenced File, Memory, Skill, and
Repository-verification clients; capability advertisement derives from those
installed adapters and fails closed when a required adapter is absent.

The file also accepts `runtime_database_url`, `resource_database_url`, and
`catalog_db` / `credential_db` / `config_db` / `admin_db` / `data_subject_db` /
`environment_db` / `sessions_db` / `captured_content_db` for
server deployments. A deployment that uses one PostgreSQL authority for the
complete AllInOne management plane may instead set `management_database_url_file`
to an operator-projected secret file; it supplies the role-owned Control,
Coordinator, and Resource stores without copying the URL into configuration. It cannot be mixed
with the per-store URL fields. Seal keys and Cloud workload credentials likewise
use their existing file-backed settings. Environment variables never select
these deployment facts.

Split Control and Coordinator processes each bind a second, private HTTP
surface. `bind` serves only public/product routes; `internal_bind` serves only
authenticated service routes. A deployment must expose each internal port
through a separate ClusterIP Service with no Ingress and restrict its callers
with NetworkPolicy.

```toml
# Control Pod: public Management API plus calls to Coordinator's private Service.
role = "control"
bind = "0.0.0.0:8080"
internal_bind = "0.0.0.0:8081"
coordinator_internal_url = "http://awaken-coordinator-private:8081"

# Control and Coordinator: the same operator-projected, least-scope token file.
executable_agent_registration_token_file = "/var/run/secrets/awaken/agent-registration-token"

# Coordinator Pod: public runtime API plus calls to Control's private Service.
role = "coordinator"
bind = "0.0.0.0:8080"
internal_bind = "0.0.0.0:8081"
control_internal_url = "http://awaken-control-private:8081"
control_service_token_file = "/var/run/secrets/awaken/control-service-token"
```

The two role examples belong in separate configuration files. Split roles fail
startup when `internal_bind` is absent, malformed, or equal to `bind`;
AllInOne and Worker reject it. Token values are loaded from files and never
appear in `awaken config`. Workers reject both private-boundary credentials and
every authority database field.
`awaken database migrate` also prepares the Coordinator-owned executable Agent
command log when `runtime_database_url` is configured.

## License

Apache License, Version 2.0. See [LICENSE](LICENSE).
