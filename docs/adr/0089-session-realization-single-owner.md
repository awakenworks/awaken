# ADR-0089: Session Realization Has One Owner

- Status: Proposed
- Date: 2026-10-09
- Builds on: ADR-0088 bounded context map (Execution owns the future
  `awaken-session-realizer`; published languages belong upstream); ADR-0056
  provisioning; the runtime-host split plan, slices S and R.
- Affects: `awaken-session-executor`, `awaken-session-realization-driver`,
  `awaken-session-application`, `awaken-session-realization-contract`,
  `awaken-provisioning-contract`; awaken-cloud only through later renames.

## Context

Realization is the lifecycle of a Session's environment: claim a lease, bind an owner,
stage resources, deliver content, renew, hibernate or restore, and clean up a terminal
environment. It is one state machine, but no crate owns it. On main `5cafd57b49`, code
whose module or file names realization, environment lifecycle, provisioning or terminal
reconciliation totals about 48,000 production lines in eight crates across four
contexts:

| Crate | Lines | Role today |
|---|---:|---|
| `awaken-session-executor` | 14,264 | decides and executes transitions inside `SharedHost` (113 `SharedHost` methods in realizer modules, 43 upward module edges into host modules) |
| `awaken-provisioning-contract` | 12,833 | provider port plus lease, spec and preparation vocabulary |
| `awaken-sandbox-local` | 3,805 | provider |
| `awaken-session-application` | 3,790 | decides realization control (`realization`, `realization/control`) |
| `awaken-session-realization-contract` | 3,599 | `FrozenSessionProjection`, five commands (`Begin`, `Renew`, `Activate`, `Acknowledge`, `Fail`), and sixteen control traits |
| `awaken-sandbox-container` | 2,973 | provider |
| `awaken-session-realization-driver` | 1,609 | executes projected actions |
| `awaken-execution-composition` | 1,303 | wires providers |

The environment state itself is decided in three places: `SessionEnvironmentState`
variants are constructed 41 times in runtime-host, 34 in the state contract and 19 in
session-application. No component can answer "what state is this environment in, and
what happens next" alone, and every fix to the lease or cleanup rules touches several
crates. The split plan's slice R moves only runtime-host's cluster; that would leave two
owners (the realizer and session-application's control logic).

## Decision

1. **One owner.** `awaken-session-realizer` (context: Execution, as ADR-0088 decision 2
   already lists) owns the realization state machine: lease, ownership, resource
   staging, content delivery, hibernation, terminal cleanup. It runs in both
   topologies, which is why it lives with Execution and not in the Session application:
   a database-less Worker realizes environments without a local Session store.

2. **Commands in, observations out.** The Session decides *that* an environment must
   exist for a frozen projection and *when* it may be released; it never decides lease
   or ownership transitions. It sends the realizer the existing commands
   (`BeginSessionRealization`, `RenewSessionRealization`, `ActivateSessionRealization`,
   `AcknowledgeSessionRealization`, `FailSessionRealization`, terminal cleanup
   assignments). The realizer reports observations (lease receipts, realized, failed,
   released, cleaned up), which the Session application records through one port. The
   sixteen control traits collapse to that command port and that observation port;
   any trait left without a second implementation is deleted.

3. **Functional core.** Transitions are one pure function over
   (realization state, command or provider event) → (new state, effects to run). The
   imperative shell executes effects against the provider port and records receipts.
   The transition function is tested with exhaustive decision tables; provider
   behaviour is tested separately against the port.

4. **What moves in.** runtime-host's realizer modules (the module-layer guard's
   `realizer` set), `awaken-session-realization-driver` (merged, not wrapped), and the
   decision logic of session-application's `realization` and `realization/control`
   modules. What stays: the Session application keeps issuing commands and recording
   observations; providers stay behind the provider port; the provisioning contract is
   reduced to that port and its spec vocabulary (with the ADR-0056 slimming).

5. **Ratchet.** Code named for realization outside the realizer, its contract and the
   providers may only shrink, counted by the same guard style as the crate budget.

6. **Order.** After the runtime-host slot is layered (slice S), so the realization
   state is already one type when it moves; this ADR is the scope of slice R.

## Alternatives

- **Move runtime-host's cluster only (the original slice R).** Leaves the control
  decisions in session-application: two owners, and every lease rule still spans two
  crates.
- **Make the Session application the owner.** Puts provider mechanics into the Session
  application, which a database-less Worker does not run; the Worker would need a second
  implementation.
- **Keep the sixteen control traits and add a facade.** Hides the spread without
  removing it; the facade would be a third place that knows the transitions.

## Consequences

- One crate answers what an environment is doing and why; lease and cleanup bugs are
  fixed in one transition function with a decision table that names the case.
- runtime-host loses its largest cluster (toward the plan's < 20,000 lines), and
  session-application loses about 3,800 lines of control logic.
- The Session ↔ realizer boundary becomes an explicit command / observation protocol,
  which is also the remote protocol a Worker uses (with the Session-owned remote ports
  of the split plan's S3).
- Persisted data is unchanged: the commands, receipts and `FrozenSessionProjection`
  keep their wire shapes.
