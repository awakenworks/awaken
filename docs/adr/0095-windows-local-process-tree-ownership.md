# ADR-0095: Windows local process tree ownership

- Status: Accepted
- Date: 2026-10-26

## Context

LocalProcess already owns local spawn, polling and termination. Its Unix adapter
signals a private process group. Windows previously terminated only the direct
child, leaving Node/ACP descendants alive after cancellation or owner exit.
Growth Desk had to supply its own Windows launcher for this missing mechanism.

## Decision

Keep the existing LocalProcess authority and its ProcessHandle contract. On
Windows, compose the exact process-wrap 10.0.1 JobObject and KillOnDrop adapters.
They assign the suspended child before workload execution and retain the private
kill-on-close Job. Application code remains subject to unsafe_code=forbid.

Restore the configured Tokio Command after spawn. Stdio stays with the same child;
no detached supervisor or process-name termination is added. Windows cancellation
remains forceful and now reaches the owned Job. A natural leader exit requests
descendant teardown. Poll still observes the leader's exit; it does not assert
that all physical descendants were observed quiescent.

Use the same owner for ACP capability negotiation, bounded host discovery and
wrapper installation. Their timeouts terminate the retained process tree and
retain their existing typed failure categories. Windows npm execution resolves
Node and the exact package manifest/bin entry, with package containment checks,
instead of executing a POSIX shell shim.

## Consequences

Unix behavior is preserved. Windows Local remains host-backed execution; this
process lifetime boundary does not supply Namespace isolation. No process receipt,
Session completion or business acceptance is synthesized by dropping a handle.

## Validation

LocalProcess tests cover cancellation, drop, natural leader exit, owner termination
without Rust Drop, pipe projection and wait/cancel interleavings. Retain the live
descendant process handle before teardown to distinguish exit from PID reuse.
ACP manifest tests cover exact pin, missing entry and path escape; composed ACP
and real Worker qualification remain separate evidence levels.
