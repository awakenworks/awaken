# Formal proof trust

## Authority and scope

The existing formal source annotations, obligations, plan and gate own proof
selection. Derive the selection from those inputs rather than maintaining a
second harness inventory or a host-specific run schedule in this document.

The formal entrypoint and its source-derived plan retain their authored
domains, tool profiles, resource limits and evidence checks. Finite exhaustive
tests are proofs only for their complete declared domain. Wide or unconstrained
inputs retain their symbolic proof owner; a small sample is not an equivalent
replacement.

## Proof families

For each obligation, retain the production entrypoint, legal input domain,
independent expectation, reachability or cover conditions, and a negative
control where applicable. A helper must not compare a production call with
itself or hide an unreachable assertion behind an exception. Share a genuine
obligation owner instead of keeping duplicate harnesses or expectation tables.

Model checking establishes the stated model and assumptions. Rust witnesses
establish their actual source selection. Neither establishes external service,
physical process or deployed-provider correctness without the corresponding
observations.

## Evidence boundaries

Use the existing source-bound evidence reader for the actual selected cases,
properties, covers, tool versions, failures and incomplete results. A resource
refusal, cancelled batch, missing artifact or partial run remains incomplete.
Historical execution packets are retained outside source control with their
original hashes. They do not qualify a rewritten or newly composed source.

The existing formal plan and coverage declarations are the inventory authority.
This document owns trust arguments, not execution counts or a list of local logs.

## Nanosecond image

The existing `unix_nanos_of_epoch_ms_is_strictly_monotone` harness now checks
the complete image of one production conversion over every `u64`, including
zero and `MAX`. Its unit oracle is
`(wire / 1000) * 1_000_000_000 + (wire % 1000) * 1_000_000`, with both terms
promoted to `u128` before multiplication. Euclidean division makes this exactly
`u128::from(wire) * 1_000_000`; the largest image is below 2^84. This stronger
equality implies the original strict-order bridge. The oracle resembles the
standard library's unit decomposition; it is not another independent
implementation. The native conversion test retains its linear oracle.

Production time code and the wire domain are unchanged. Expiry `Ord` has the
independent compositional check described below. The two successor covers still require `wire < u64::MAX`,
because `MAX` has no successor; separate zero/MAX covers exercise the full
image's endpoints. The harness selects the existing `solver-z3-v1` profile,
with all checks and the original 1,800-second ceiling retained.

## Expiry ordering

The existing `expiry_from_epoch_ms_is_strictly_monotone` harness checks two
links rather than constructing two adjacent millisecond conversions. First,
every `u64` wire, including zero and `MAX`, must construct the same complete
Expiry as `Expiry::at(UtcTime::from_epoch_ms(wire))`. Second, the real Expiry
comparison over two independent canonical `Duration` coordinate pairs must
equal their integer lexicographic comparison. Both seconds inputs range over
all `u64`; only the `u32` nanos inputs are bounded below 1,000,000,000, the
valid representation of every Duration. No wire or seconds bound is added.
The two explicit nanos assumptions define only that canonical representation;
they do not assume an ordering or exclude any valid Duration. They replace
surjective modulo generators over precisely the same legal domain.

The composition uses the actual single-field Expiry/UtcTime wrappers,
`Duration::new` preserving canonical inputs, and the actual `as_nanos`
projection `P(s,n) = u128(s) * 1_000_000_000 + u128(n)`. This projection is an
order embedding: equal seconds compare their nanos, and a one-second increase
exceeds the largest possible reversed nanos difference by at least one.
Its largest value is below 2^94. The full-width nanos-image harness already
checks `P(from_epoch_ms(w)) = u128(w) * 1_000_000`; together these links prove
the original strict successor law and the order of every millisecond pair.
The image harness carries the same two obligation anchors, without a second
formula or proof owner. A changed wrapper or canonical representation requires
this composition to be reviewed again; an opaque image equality alone would
not prove the actual Ord implementation.

## Fixed-target checkpoints

The retained scalar formulation is the planner table plus its full-width
successor replay and the independent stale-fence harness. The table draws
`deadline`, `target` and `now` independently from `{0, 1, 2, 3}`.
A single per-triple map fixes zero and maps its at most three positive
classes to their ranks `1, 2, 3`; this covers all 26 realizable
joint zero/weak-order classes, not merely separate pairwise comparisons.
The production plan copies the target, so input and output use that same
injective map and its inverse. Revision, phase, attempt presence and the
original matching-tenure epoch bound are unchanged; replay keeps its
full-width inputs and both live/expired covers, and stale-fence keeps its
foreign-coordinate/full-epoch domain and four covers.

The only time-image dependency is the existing
`unix_nanos_of_epoch_ms_is_strictly_monotone` at its lease-contract owner,
now also linked to the BackgroundTask obligation by its original anchor.
The independent Expiry `Ord` lemma is not a precondition for this checkpoint
correspondence. No proof or formula is duplicated and no domain or solver
change is made to the retained replay or stale harness.

## Total helper postconditions

The Dispatch proof now compares the complete exact-claim outcome with its
independent expected DeadLetter before inspecting the payload. Its cancellation
preservation, non-requeue and non-claim assertions remain. The adjacent equality
of an execution-admission call with itself is removed: the existing physical-slot
and thread-peer proofs already own those execution-admission rules. Production
reducers, wire types and admission rules are unchanged.

The two Vault no-entropy proofs use a persistent invocation flag instead of
incrementing a counter in a factory that must never execute. Any invocation
still fails; the separate exactly-once proofs retain their counters. The
config-resolver proof combines every original outcome and payload condition
into one total relation; unsupported error variants still make it false.
These corrections retain the existing input domains and production decision
owners.
