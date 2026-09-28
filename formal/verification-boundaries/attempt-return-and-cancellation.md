# Attempt return and cancellation verification boundaries

- Cowork cancellation and credential rollout extend the existing Dispatch,
  Runtime context, ACP supervisor and Vault outbox owners. Lease-request timeout
  retains the owned cleanup observer; Native tool abandonment and ACP reap failure
  veto physical ACK without rewriting the business outcome. Tests distinguish
  cancellation admission, executor return, physical termination and settlement.
  Native opaque-tool cancellation without a terminal receipt can still block
  same-Thread resumption indefinitely; no recovery API or receipt is invented.
  The one attempt-local observer counts active participants and atomically closes
  first-poll entry before physical ACK. A returned parent drains entered children;
  delayed entry is refused before its effect is polled, and any dropped/unknown
  participant retains the veto. Continuation permission is separate from return
  permission so an observed inline child may run its own nested model/tool work.
  Gates, model preparation/admission, phase hooks and run-end guards use the same
  observer. The two pure Kani kernels cover every u64 count and Boolean state;
  Mutex/Notify scheduling, unobserved adapter work and durable hard-crash recovery
  remain outside those scalar proofs. Test comments own the 30-row local table
  and the Memory/SQLite parent-return-before-child decision rules.
  Pure Native inference returns data, and its discarded response cannot invoke
  a governed tool. Without execution-termination evidence, abandoned inference remains
  Indeterminate and keeps its physical fence. Neither this test nor the model
  proves that provider computation/billing stopped.
  The same owned drive retries an observed-return ACK after transient write
  failure, response loss or request timeout, without rerunning execution; a
  permanent rejection remains an error. Tests inject all four outcomes into
  the existing Dispatch store fixture. Progress requires transport recovery
  while the observer remains alive; process death before ACK is not repaired by
  this local retry or proved away by scalar fencing.
  See ADR-0024 D5. Docker, Podman and Kubernetes share a real process oracle for
  Pending-wait cancellation, concurrent wait/poll/signal, TERM and KILL; these
  tests do not establish termination of an opaque remote provider operation.
  AcpBoundary and SessionThreadToolComposition explore bounded observer/crash
  schedules, and ManagedCredentialRollout explores independent busy/failing
  targets with exact all-target ACK. Its progress property assumes eventual
  stable process/target availability, not merely intermittent availability.
  Four semantic mutants reject timeout-as-drop, signal-as-receipt,
  crash-as-quiescence and first-target-as-convergence. Scalar Kani fences remain
  direct kernel proofs; whole async lifecycle obligations retain abstract-model
  grades. Test comments own cause/effect decision rules, without another case
  ledger or a claim of universal Rust-to-model refinement.
