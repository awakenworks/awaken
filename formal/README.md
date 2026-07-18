# Formal verification

This directory complements the executable Rust model checks. It deliberately
models runtime and run ingress only; WorkQueue is out of scope.

## Kani

Proof harnesses live beside the production functions they verify under
`#[cfg(kani)]`:

- `awaken-agent-contract`: terminal absorption and typed `RunDisposition`
  state/ticket coherence, including the complete legacy 3×3 product.
- `awaken-session-contract`: `StepOutcome` constructors exclude `Running`, keep
  pending data off ended outcomes, and derive failure only from
  `EndCause::Error`.

Install and run using the official Kani workflow:

```sh
cargo install --locked kani-verifier
cargo kani setup
scripts/ci/check_formal.sh
```

The script names each harness explicitly so Kani analyzes only the finite
state kernels under proof, rather than unrelated serialization internals.

## TLA+ / TLC

`tla/RunIngress.tla` models the joint runtime/dispatch state machine with two
owners, lease-epoch fencing, crash reclaim, await/wake, finish, cancellation,
dead-lettering, and supersession. TLC checks:

- ticket iff the run is `Awaiting`;
- `Ended` is absorbing;
- a leased dispatch has exactly one owner;
- run and dispatch states remain coherent;
- stale epochs have no state-changing settle transition.

The checked configuration bounds `leaseEpoch` at 4. This is an explicit TLC
finite-model bound, not an unbounded temporal proof; it covers repeated wake and
reclaim cycles while keeping CI deterministic.

With Java 11+ and `tla2tools.jar`:

```sh
java -XX:+UseParallelGC -jar /path/to/tla2tools.jar \
  -config formal/tla/RunIngress.cfg formal/tla/RunIngress.tla
```

The model is a safety specification. It intentionally makes no liveness claim
that external input, an available owner, or a successful backend will eventually
arrive.
