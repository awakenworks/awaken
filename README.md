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
awaken worker --config /etc/awaken/config.toml --server http://coordinator
awaken config                        # effective config and database paths
awaken config --json                 # redacted machine-readable report
awaken --version
```

Production configuration comes only from an explicit `--config` path or the
standard `~/.awaken/config.toml`, followed by typed defaults. `AWAKEN_*`
environment variables are not deployment, model, business, or credential
configuration sources.

All embedded databases and the generated `control-seal.key` live under the one
data directory. Database schema migrations run automatically, under the
existing migration locks, before the server accepts traffic. Shared deployments
select Postgres with `runtime_database_url`, `resource_database_url`, and the
per-component control database fields shown by `awaken config`; database URLs
and key material are always redacted.

The typed configuration file can hold bootstrap settings. For example:

```toml
data_dir = "/srv/awaken"
bind = "127.0.0.1:8080"
mode = "local"
run_local_pool = true
no_browser = false
```

The file also accepts `runtime_database_url`, `resource_database_url`, and
`catalog_db` / `credential_db` / `config_db` / `admin_db` / `sessions_db` for
server deployments. A deployment that uses one PostgreSQL authority for the
complete management plane should instead set `management_database_url_file` to
an operator-projected secret file; it supplies all five control stores and the
Resource Plane without copying the URL into configuration. It cannot be mixed
with the per-store URL fields. Seal keys and Cloud workload credentials likewise
use their existing file-backed settings. Environment variables never select
these deployment facts.

Split Control and Coordinator processes additionally use one explicit private
registration boundary:

```toml
# Control: destination of immutable executable Agent registrations.
coordinator_internal_url = "http://awaken-coordinator:8080"

# Control and Coordinator: the same operator-projected, least-scope token file.
executable_agent_registration_token_file = "/var/run/secrets/awaken/agent-registration-token"
```

The token value is loaded from the file and never appears in `awaken config`.
Workers reject this registration credential and every authority database field.
`awaken database migrate` also prepares the Coordinator-owned executable Agent
command log when `runtime_database_url` is configured.

## License

Apache License, Version 2.0. See [LICENSE](LICENSE).
