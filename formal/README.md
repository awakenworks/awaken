# Formal verification

This directory complements the executable Rust model checks. It models Runtime,
Run Ingress, and first-class delegated child Runs; WorkQueue remains out of scope.

## Kani

Proof harnesses live beside the production functions they verify under
`#[cfg(kani)]`:

- `awaken-agent-contract`: terminal absorption, typed `RunDisposition`
  state/ticket coherence, and the production child-result handoff kernel. The
  latter proves that only `Pending` can become `Delivered`, an ended parent never
  records or delivers a result, and the two exactly-once effects have unique
  preconditions.
- `awaken-session-contract`: `StepOutcome` constructors exclude `Running`, keep
  pending data off ended outcomes, and derive failure only from
  `EndCause::Error`.

Install and run using the official Kani workflow:

```sh
cargo install --locked kani-verifier
cargo kani setup
scripts/ci/check_formal.sh
```

The script names each harness explicitly so Kani analyzes only the finite state
kernels under proof, rather than unrelated serialization internals.

## TLA+ / TLC

`tla/RunIngress.tla` models the joint Runtime/Dispatch state machine with two
owners, lease-epoch fencing, crash reclaim, await/wake, finish, cancellation,
dead-lettering, requeue, and supersession. TLC checks:

- ticket iff the Run is `Awaiting`;
- `Ended` is absorbing;
- a leased dispatch has exactly one owner;
- Run and Dispatch states remain coherent, including `DeadLetter` as a
  recoverable dispatch condition rather than an Agent outcome;
- stale epochs have no state-changing settle transition.

`tla/Delegation.tla` composes one parent with local, remote, cyclic, and too-deep
child candidates. It checks two independent process owners and verifies:

- stable first-class child membership and bounded parallel/total creation;
- depth and lineage-cycle guards;
- independent parent/child crash recovery through monotonic epochs;
- durable `Pending` between child end and parent result commit;
- result delivery count never exceeds one and late results are discarded;
- an ended parent has no pending result and persists cancellation until child
  acknowledgement;
- Local and Remote children share the same transition system—routing kind is
  deliberately not an input to any lifecycle action.

The configurations bound ingress `leaseEpoch` at 4 and delegation epochs at 2,
with at most two created children. These are explicit TLC finite-model bounds,
not unbounded temporal proofs. They cover all reachable interleavings within
those bounds while keeping CI deterministic.

With Java 11+ and `tla2tools.jar`:

```sh
java -XX:+UseParallelGC -jar /path/to/tla2tools.jar \
  -config formal/tla/RunIngress.cfg formal/tla/RunIngress.tla
java -XX:+UseParallelGC -jar /path/to/tla2tools.jar \
  -config formal/tla/Delegation.cfg formal/tla/Delegation.tla
```

Both models are safety specifications. They intentionally make no liveness claim
that external input, an available owner, a network response, or a successful
backend will eventually arrive.

## Coverage boundary

The checked claims have three different strengths and must not be conflated:

- Kani proves the production pure transition functions for every symbolic input
  in their finite Rust types.
- TLC exhausts every interleaving in the configured finite models, but there is
  not yet a machine-checked refinement mapping from the full async Rust system to
  those TLA+ actions.
- executable tests connect stable child identity, ordinary Runtime behavior,
  nested origin depth, per-Agent rosters, cancellation-token direction, and the
  SQLite/Postgres CAS repositories to production adapters.

The `DelegationStore` repositories are durable and revision-fenced, but the host
does not yet coordinate a `DelegationGroup` update, child dispatch, and the
parent's `ThreadCommit` in one transactional workflow. Consequently the model
proves the desired `ResultPending -> Delivered` protocol, while end-to-end
exactly-once parent delivery is not yet a machine-checked property of the full
deployment. External tool side effects, network adapter correctness, database
engine correctness, unbounded state spaces, fairness, and eventual delivery also
remain outside the formal proof boundary. They require idempotency contracts,
integration/fault-injection tests, or a future refinement proof rather than a
larger finite TLC bound alone.
