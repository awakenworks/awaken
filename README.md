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
awaken start
```

Running `awaken` without a command is the same as `awaken start`: it starts the
API, embedded web console, and local worker, persists all local state under
`~/.awaken`, and opens `http://127.0.0.1:8080`. Use `--port`, `--data-dir`, or
`--no-browser` for common local overrides. The resulting binary contains the
complete console and needs no external web directory or Node.js at runtime.

Useful operational commands:

```console
awaken serve                         # foreground, headless/service-manager mode
awaken worker --server http://host   # join a control plane
awaken config                        # effective config, database paths, env reference
awaken config --json                 # redacted machine-readable report
awaken --version
```

Configuration precedence is command line, then `AWAKEN_*` environment, then
`~/.awaken/config.toml`, then defaults. `AWAKEN_DATA_DIR` and `AWAKEN_BIND` are
the canonical environment names; the previous storage/listen names remain
one-release aliases and print migration warnings.

All embedded databases and the generated `control-seal.key` live under the one
data directory. Database schema migrations run automatically, under the
existing migration locks, before the server accepts traffic. Shared deployments
select Postgres with `AWAKEN_RUNTIME_DISPATCH_DATABASE_URL`,
`AWAKEN_RESOURCE_DATABASE_URL`, and the per-component control database settings
shown by `awaken config`; database URLs and key material are always redacted.

An optional `~/.awaken/config.toml` can hold stable, non-secret bootstrap
settings. For example:

```toml
data_dir = "/srv/awaken"
bind = "127.0.0.1:8080"
mode = "local"
run_local_pool = true
no_browser = false
```

The file also accepts `runtime_database_url`, `resource_database_url`, and
`catalog_db` / `credential_db` / `config_db` / `admin_db` / `sessions_db` for
server deployments. Prefer environment variables or a secret manager for URLs
that contain credentials and for seal keys.

## License

Apache License, Version 2.0. See [LICENSE](LICENSE).
