# ACP handshake and wire verification boundaries

This file owns the detailed boundaries for ACP handshake completion and duplex
wire behavior. The architecture-slice index links here instead of duplicating
these claims.

- ACP prompt admission consumes one private `PromptReady` witness produced by
  the existing `run_turn_with_config` handshake owner only after initialize
  (including optional authentication), new/load, capability validation, and
  every selected configuration request has completed. Kani exhausts the
  production three-step phase reducer and proves that its admission predicate is
  true only in the exact configured phase. The private Rust type boundary alone
  constructs the witness from that predicate; the existing eighteen-row Tokio
  table crosses explicit failure and timeout at every concrete request boundary
  and observes no prompt or Ready event. The proof does not establish Tokio
  timer accuracy, scheduler progress, remote-agent behavior, arbitrary option
  counts, process termination, or universal Rust-to-TLA refinement. Those remain
  executable or external evidence, and `AcpBoundary` stays the bounded protocol
  cross-check.
- ACP crossing writes extend the existing `AcpBoundary` model with a bounded
  duplex schedule, FIFO read-ahead, cancellation and explicit overflow. Kani
  proves the production buffer admission arithmetic over `usize` inputs; it does
  not prove Tokio readiness, UTF-8 parsing, Kubernetes transport or CLI progress.
  The actual wire tests cover concurrent large writes, cancellation within a
  UTF-8 frame, EOF, malformed input and flooding; the duplex mutant must fail.
  Cleanup drains trailing bytes without turning EOF into exit evidence; a missing
  Kubernetes process status remains an error. These async/adapter transitions
  are covered by executable tests, not by the buffer-arithmetic proof.
