# Awaken Runtime

This repository hosts the `awaken-runtime` design corpus and guardrails. The
runtime protocol/specification and conformance surface are licensed under
Apache-2.0; code packages may use their own file or package license metadata.

Start with [docs/README.md](docs/README.md) for the bounded contexts, runtime
coverage target, architecture invariants, and documentation checks.

## Quick start

Build and launch the management API and web console on one port:

```console
cargo run -p awaken-cli --bin awaken -- start
```

Open `http://127.0.0.1:8080`. The command builds `web/dist` with `pnpm` when it
compiles the `awaken` executable; the resulting binary contains the complete
production console and needs no external web directory or Node.js at runtime.
Set `AWAKEN_HTTP_ADDR` to change the listener. Running `awaken` without `start`
keeps the API-only behavior.

## License

Apache License, Version 2.0. See [LICENSE](LICENSE).
