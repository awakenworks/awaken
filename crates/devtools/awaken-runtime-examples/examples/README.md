# Runtime examples

Runnable, teaching-first examples of assembling and running the `awaken` runtime.
Each example is a single, heavily commented `main` that wires the runtime's ports
(model, tools, permission gate, commit boundary) into a working run.

This crate is the **composition root**: the one place allowed to name every
concrete adapter. Everything below it depends only on neutral ports, so an example
swaps the stub model for a real one by changing a single line.

## Examples

| Example | Teaches | Run |
|---|---|---|
| `direct_runtime` | Build the catalog and executable snapshot **by hand** (no config store), wire the ports, execute one run, read the committed transcript. | `cargo run -p awaken-runtime-examples --example direct_runtime` |

(More to come — e.g. producing the snapshot through `awaken-config-store` instead
of building it by hand.)

## Notes

- Examples use a deterministic stub model (`ScriptedLlm`), so they run with no API
  key. To use a real model, swap `ScriptedLlm` for
  `awaken_provider_genai::GenAiExecutor::new()` and set the provider key in the
  environment.
- Each example asserts its outcome and has a smoke test under `tests/`, so a
  regression turns `cargo test` red.
