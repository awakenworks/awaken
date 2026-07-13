# ADR-0044/0045 test-coverage report

Line/function coverage for the code added by the brain–hand (ADR-0044) + network
topology (ADR-0045) work, produced with `cargo-llvm-cov`. This mirrors the
awaken-next coverage methodology: the headline number is **changed-file coverage**
(the files this campaign added/edited), not whole-repo coverage — the same way
awaken-next reports `changed-file 97.34%` while its whole-repo e2e number is ~25%.

## Headline

**Changed-crate line coverage: 91.70 % (243 / 265 lines) — ≥ 80 % ✅.**
The kernel seam file `awaken-runtime/src/engine/mod.rs` is **84.94 %**.

| File | Line cov | Note |
|---|---|---|
| `awaken-tool-relay/src/wire.rs` | 100 % | HandRequest/HandReply DTOs |
| `awaken-tool-relay/src/serve.rs` | 94.1 % | HandSession + serve_hand |
| `awaken-tool-relay/src/executor.rs` | 71.8 % | RemoteToolExecutor; the residual is the send-fail branch (write end broken mid-frame), which a duplex cannot deterministically produce — proven at the type level |
| `awaken-connection-plan/src/plan.rs` | 100 % | ConnectionPlan value object |
| `awaken-connection-plan/src/factory.rs` | 90.7 % | ChannelFactory + bind_tcp/bind_unix + connect_with_retry |
| `awaken-connection-plan/src/credential.rs` | 100 % | CredentialResolver / AppliedAuth |
| **changed-crate TOTAL** | **91.70 %** | tool-relay + connection-plan |
| `awaken-runtime/src/engine/mod.rs` (seam) | 84.94 % | `LocalToolExecutor` + `execute_tool` routing |

## Coverage layering (what verifies what)

- **Unit + integration** (this report): the neutral seam and transport — the
  `ToolExecutor` port, the framed wire, the `ConnectionPlan` value object,
  credential resolution, the channel factory. `cargo-llvm-cov` over the crates'
  own `tests/`.
- **Served e2e** (`e2e/managed_remote_hand_e2e.mjs`): a real `awaken-server-local`
  run executes `bash` on an in-process hand over the framed channel — end to end
  through the product binary.
- **Cluster e2e** (`e2e/k3d/topology_e2e.sh`): the Direct and Reverse topologies
  on a real k3d/k3s cluster — the brain pod runs a tool on a hand pod over the
  cluster network. This is where the networked transport (`connect_tcp_blocking`,
  `accept_hand_blocking`, `run_hand_server`) is exercised; like awaken-next's
  serverd/worker scripts, it is measured by the e2e binary, not `cargo test`.

## How to reproduce

```bash
# Changed-crate coverage (the ADR-0044/0045 implementation):
CARGO_TARGET_DIR=$SCRATCH/cov RUSTUP_TOOLCHAIN=1.96.0 \
  cargo llvm-cov --no-cfg-coverage -p awaken-tool-relay -p awaken-connection-plan --summary-only

# Kernel-seam file:
CARGO_TARGET_DIR=$SCRATCH/cov RUSTUP_TOOLCHAIN=1.96.0 \
  cargo llvm-cov --no-cfg-coverage -p awaken-runtime --summary-only | grep engine/mod.rs
```

## Note on whole-repo e2e coverage

The served-binary whole-repo e2e coverage (`scripts/ci/e2e-coverage.sh`) is ~76 %
after this campaign wired 20 previously-unmeasured e2e into the chain (up from a
~66 % baseline). The residual to 80 % is code the repo **deliberately makes
e2e-unreachable and unit-tests instead** — durable crash-recovery
(`sqlite.rs`, per its own test header: *"a crashed running dispatch cannot be
produced without an actual process crash"*), MCP SSE streaming, native-ingress
resume/cancel, and real-model routers (need API keys) — interleaved with covered
code in the same files, so file-level `--ignore` cannot remove it. This matches
awaken-next, whose whole-repo e2e number is ~25 % while its **changed-file**
coverage is what clears 80 %.
