# ADR-0103: Running Budget Tightening, Stop Receipts, Classified Adapter Faults and Cross-Base Realization Takeover

- Status: Accepted
- Date: 2026-10-31

## Context

Binding the last design-only Session execution scenarios to evidence recorded
six new product gaps in the acceptance ledger, each with an ignored reproducer:

- T02-C03: the `compile_fail` doctest pins in `awaken-runtime`
  `conversation.rs` were not enforced. Stable rustdoc ignores a pinned E-code,
  so a doctest that fails for an unrelated reason still passed. The C2 pin
  (E0412) was also stale: rustc 1.96 reports E0425.
- T13-C02: a Running Session could not be tightened before its next model
  effect, because `update_limit` and the update command required an Idle root.
- T25-C01: redacting an immutable Memory version mapped to `workspace.write`,
  the same action as a content write.
- T31-C03: a takeover process with its own data_dir found no realization marker
  for the dead process's local sandbox, reported `Incompatible`, and terminated
  the Session instead of finishing the held Run.
- T32-C01: the AI SDK and AG-UI explicit Stop answered 204 with no receipt, so
  two Stops were not two attributable commands.
- T33-C03: MCP, ACP and the Hand relay dropped the authority, uncertainty or
  retry hint that the local and HTTP Worker paths keep.

## Decision

### D1 The doctest gate checks pinned error codes

The workspace doctest step (`scripts/ci/check-rust.sh`, and the coverage
doctest run) runs under `RUSTC_BOOTSTRAP=1`, which makes rustdoc check each
`compile_fail,E....` pin. The C2 pin is corrected to E0425. The SDK consumer
acceptance test reads the doctest step's environment from the gate script, so
it checks the mutants exactly as the gate runs them.

Every pinned doctest in the workspace is now checked. A stale pin elsewhere
fails the gate instead of passing silently; fixing it is the intended outcome.

### D2 Tightening an Active budget is admitted on a live root

`SessionBudgetOperations::update_limit` admits a lower threshold for an Active
budget whenever the root is not terminal. Loosening and removal still wait for
Idle. The update command admits a budget-only tightening (no title, metadata,
tools or MCP change) while a Run executes. The next model request is then
decided under the current limit, and the frozen model and price pins are
unchanged.

### D3 Memory version redaction is its own action

`POST /v1/memory_stores/{id}/memory_versions/{vid}/redact` maps to
`workspace.memory_redact`. A principal granted only `workspace.write` cannot
redact history. Roles holding the `workspace.*` pattern (hosted admin,
builder, publisher) keep the authority.

### D4 A takeover rebuilds a realization whose marker lives in another base

A filesystem (Workdir/Namespace) realization belongs to the provider base
where its marker lives. Suppose a process observes a current handle with no
marker under its own base, and finds no root and no private stage for the
handle's incarnation. It then reports exact absence (`DefinitivelyUnavailable`
with no incarnation) instead of `Incompatible`.

The existing aggregate Rebuild transition then re-realizes the Environment on
the current process, with a fresh marker and physical incarnation. The
provider accepts a rebuild from such an unmarked source only under two
conditions:

- the aggregate effect is a distinct, authorized successor of the source
  handle's effect;
- the fingerprint is the same.

Lease and incarnation fencing therefore still stops a stale owner, and a live
owner's realization is never adopted, reused or deleted across bases. Any
unmarked physical entry stays incompatible, because its owner is unproven.

A same-base handle whose marker and root were both deleted is now rebuilt
instead of terminating the Session.

### D5 An explicit Stop returns its own receipt

`RunApplication::stop` interrupts the Run and returns a `StopReceipt { id,
thread_id }`. AG-UI `DELETE /v1/ag-ui/threads/{t}/runs` and AI SDK `DELETE
/v1/ai-sdk/threads/{t}/runs` answer 202 with the receipt. The id joins a
per-process random nonce with a per-process sequence: it does not repeat
within a process and does not collide across restarts. It is a receipt for
the command, not a durable Event; a passive stream hang-up still produces no
receipt.

### D6 Adapter faults keep their class

- MCP: `tools/call` answers a certain tool fault with an implementation-defined
  JSON-RPC code: -32001 unavailable before dispatch, -32002 definitive
  failure, -32003 state invalidated after dispatch. An unknown outcome keeps
  -32603, so a client that does not know the codes still treats it as unknown.
  The in-repo client (HTTP and stdio) maps the three codes back to the same
  `ToolError` class.
- ACP: the classifier distinguishes `Overloaded` (529, with its retry hint)
  and `ConnectionDropped` (an in-flight Prompt drop). `failure_cause` keeps
  the native codes: `overloaded`, `rate_limited` and `provider_error`, with
  the retry hint in the message. An in-flight drop becomes `Indeterminate`.
- Hand relay: `HandErrorKind::UnavailableBeforeDispatch` carries a certain
  not-run fault, and the brain-side executor maps it back to the retry-safe
  class.

## Consequences

- The six reproducers are un-ignored, and their cases are
  `source_binding_only`. A case moves to `current_clause_complete` only after
  the batched native Windows execution registers its witness.
- New wire values: three MCP JSON-RPC codes, the `unavailable_before_dispatch`
  Hand error kind, and the Stop receipt body. Older peers that do not know the
  MCP codes keep the previous behaviour; a Hand and brain must run the same
  release to exchange the new kind.
- The D1 gate may expose other stale doctest pins on its first batched run.
