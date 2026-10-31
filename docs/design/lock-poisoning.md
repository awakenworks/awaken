# Lock poisoning

Principle P7 (concurrency you can reason about) and plan v5 constraint S5: a
lock's poisoning is handled by the scope of the invariant it guards. The
answer is chosen once, by the type of the lock, and no call site restates it.

## Two lock semantics

A `std::sync` lock is poisoned when a holder panics while holding it.

| Semantics | Types | Guards | Poisoned acquisition |
|---|---|---|---|
| Invariant | `InvariantLock<T>`, `InvariantRwLock<T>` | a critical section that keeps an invariant across fields or entries: a ledger, a state-machine transition, a map with its index, a check-then-update rule | `Err(LockPoisoned)`; the caller propagates it as its own typed error |
| Leaf | `LeafLock<T>`, `LeafRwLock<T>` | state valid after any panic: a cache, a counter, a registry of independent entries, a recorded-call log, a slot replaced whole, a `()` barrier | not observable: the guard is recovered (`PoisonError::into_inner`) |

`LockPoisoned` names the guarded type. Each crate decides the error class once,
in its `From<LockPoisoned>` conversion (or the `map_err` of an existing typed
refusal), never at the call site. The invariant types keep the `std` signatures
(`lock() -> Result<Guard, _>`, `try_lock`, `get_mut`, `into_inner`,
`is_poisoned`), so code that already refused poison (`.lock().map_err(..)?`,
`if let Ok(..)`) keeps its shape; the leaf types return the guard directly.

The realization path's existing refusals are this pattern specialised:
`RealizationLock`, `ExecutorLock` and `DeliveryLock` are the classes those
crates map an invariant lock's refusal to. Their locks become `InvariantLock`s
and their codes stay unchanged.

Classification rule: a lock is a leaf only when every critical section on it is
one read or one whole mutation of the guarded value (or of one independent
entry) and calls no foreign code. Anything else, and anything ambiguous, is
invariant. Test doubles in testkit and `test-support` code are leaves: their
state is a recording, and a panic in a test is already the test's failure.

## Where the types live (S4)

The types live in a new depth-0 SDK leaf, `crates/runtime/awaken-sync`
(`authority = "lock-poisoning"`, `separation = "contract"`). S4 cuts *contracts*
by bounded context and caller; the poisoning law is no context's vocabulary and
no boundary between contexts. It is a language-level law every context obeys,
like the expiry law of `awaken-time`, and ADR-0091 D2 keeps "a small vocabulary
read by many crates" a leaf. No existing crate fits: every crate holds one
`authority`, and `awaken-time`'s is the wall clock. The crate takes the
`contract` layer only because contract crates (`awaken-runtime-contract`,
`awaken-provisioning-contract`) hold locks and a contract may depend only on
contracts; it raises the contract-crate count by one, recorded with its reason.
When `std::sync::nonpoison` is stable, `LeafLock` becomes that type and the
crate shrinks to the invariant half.

## Enforcement (S1)

The compiler enforces the rule: production code cannot read a `std::sync` lock
except through `awaken-sync`. V1 installs this `clippy.toml` (this lane does not
create the file):

```toml
# Plan v5 S5 / P7: a lock's poisoning is decided by its type, not at the call
# site. See docs/design/lock-poisoning.md.
disallowed-methods = [
  { path = "std::sync::Mutex::lock", reason = "use awaken_sync::InvariantLock (refuses poison) or LeafLock (recovers it)" },
  { path = "std::sync::Mutex::try_lock", reason = "use awaken_sync::InvariantLock or LeafLock" },
  { path = "std::sync::Mutex::get_mut", reason = "use awaken_sync::InvariantLock or LeafLock" },
  { path = "std::sync::Mutex::into_inner", reason = "use awaken_sync::InvariantLock or LeafLock" },
  { path = "std::sync::RwLock::read", reason = "use awaken_sync::InvariantRwLock (refuses poison) or LeafRwLock (recovers it)" },
  { path = "std::sync::RwLock::write", reason = "use awaken_sync::InvariantRwLock or LeafRwLock" },
  { path = "std::sync::RwLock::try_read", reason = "use awaken_sync::InvariantRwLock or LeafRwLock" },
  { path = "std::sync::RwLock::try_write", reason = "use awaken_sync::InvariantRwLock or LeafRwLock" },
  { path = "std::sync::RwLock::get_mut", reason = "use awaken_sync::InvariantRwLock or LeafRwLock" },
  { path = "std::sync::RwLock::into_inner", reason = "use awaken_sync::InvariantRwLock or LeafRwLock" },
]
```

`awaken-sync` is the one crate that calls these methods
(`#![allow(clippy::disallowed_methods)]` in its root). V1 must confirm with a
canary that the paths resolve on the pinned toolchain: since Rust 1.84 `Mutex`
is defined in `std::sync::poison` and re-exported, and if clippy does not follow
the re-export the paths become `std::sync::poison::Mutex::lock` and so on.

**Test policy.** The rule binds what ships: library and binary targets with all
features, which includes testkit and `test-support` code. Test code
(`cfg(test)` modules, `tests/`, benches, examples) may use `std` locks directly,
because a poisoned lock in a test is a panic and a panic is how a test reports
failure. V1 wires this as two clippy invocations over the existing graph:

```sh
cargo clippy --workspace --all-features --lib --bins --locked -- -D clippy::disallowed_methods
cargo clippy --workspace --all-features --all-targets --locked -- -D warnings -A clippy::disallowed_methods
```

## `_poisoned_lock_fitness.py`

The clippy rule replaces the guard's L1 for `std` locks across the whole
workspace, wider than the guard's executor-and-realization scope. It does not
replace two semantic spellings: an `InvariantLock` refusal unwrapped with
`.lock().expect(..)` / `.unwrap()` (L1 on the new type), and a typed registry
refusal expected away (L2). Clippy cannot tell those apart from legitimate
`Result::expect`. So the guard stays. V1 must: widen its scan from the two
`authority` crates to all production code (after this migration the count is
zero, so it remains a no-baseline deny check), name `InvariantLock` in its
messages, and record its retirement condition: retire when the invariant
refusal can be made unwrappable only through a typed route.

## Classification

Plan v5 §4.2 counted 640 production `.lock()/.read()/.write()` calls followed by
`expect`/`unwrap` (the `production_rust` parser over the production module
closure). Classified per lock declaration: 165 leaf, 58 invariant, 4 left on
`std` pending a decision (below). Per call site, about 473 leaf and 160
invariant (resolved through the field's new type), 8 on the four open locks.

| Crate group | Leaf / invariant locks | Reason |
|---|---:|---|
| Testkits and test-support doubles (protocol-managed-testkit, mcp-server-testkit, reliability-testkit, eval, scenario-host, session-environment and worker-fleet test support, in-memory memory/skill/model/data-subject stores, work-store queue, dispatch-erasure reference authority) | 74 / 0 | recorded calls and fixture state; a panic in a test is already its failure |
| In-memory and durable repositories (credential-vault, sandbox-policy-store, executable agent/environment catalogs, env-store, store-inmem, store-sqlite/postgres/fs, dispatch memory store, dream process store) | 12 / 11 | one lock keeps several maps or a revision and its index consistent inside a transaction |
| Session realization, executor and delivery (session-realization, session-executor, session-run-delivery, credential-materializer, dispatch-execution recovery) | 2 / 21 | the registries already refused poison with `RealizationLock`/`ExecutorLock`/`DeliveryLock`; their refusals and codes are unchanged |
| Sandbox providers and the Worker (sandbox-local, sandbox-container, sandbox-memoryd, tool-relay, local-process, awaken-worker, process-win32, sandbox-kubernetes) | 24 / 10 | pool capacity with membership, the FUSE inode table, mount layouts, operation ledgers and authority proofs are invariant; owned-path sets, broker slots, LRU caches and attempt logs are leaves |
| Coordinator applications and roots (deployment-application, coordinator, process-startup, service-lifecycle, session-application) | 17 / 6 | deployment/run maps and cloud-login state change together; `Option` slots replaced whole, drain slots and diagnostics are leaves |
| Protocol and access adapters (protocol-managed, protocol-mcp, protocol-a2a, mcp-server-core, session-mcp, authz-enforce, control, config-resolver, worker-transport-security, run-executor-acp, captured-content-store) | 18 / 8 | the managed Session projection, relay routes (url + bearer + generation), session tokens with policy and the remote gate are invariant; sender registries, credentials slots and rate-limit buckets are leaves |
| SDK runtime and extensions (awaken-runtime, ext-mcp, ext-background-task, ext-skills, ext-builtin-tools, observability, runtime-contract, provisioning-contract, sqlite-pool, environment-image-build) | 18 / 2 | continuation buffers, MCP slots, task entries and counters are leaves; the memory reconciliation acknowledgement and the effect authority keep their refusal |

Observable behaviour changes (panic becomes a typed refusal, or a former typed
refusal on a leaf lock becomes a recovery) are listed in the commit series; the
notable ones are the managed Session store (`StateError::Run(Unavailable)`),
the deployment application (`Unavailable`), session tokens
(`SessionStorageUnavailable`, `authorize` denies), the session-mcp relay
(an unknown route answers 404) and the FUSE inode table (`EIO`).

### Open locks

These four still read a `std` lock in production; V1 cannot enable the lint
until each is decided:

| Lock | Why it is open |
|---|---|
| `awaken-runtime-contract` `LiveInbox` state | invariant (`offer` moves the id, entries and version together), but all 13 public SDK methods return plain values; treating a poisoned inbox as closed needs `version()` to become `Option<u64>` (SDK API change, public-api rebless) |
| `awaken-control` `ManagementAuthz.state` | the type `Arc<std::sync::Mutex<LocalIamState>>` is fixed by `awaken_iam_host::IamGate::from_local_state`; changing it is an awaken-iam API change |
| `awaken-sandbox-control-transport` `SandboxControlPublicationSlot.state` | invariant for reserve/update, yet `close()` deliberately recovers poison so disposal stays absorbing (test PS5); it needs an explicit "dispose despite poison" route on `InvariantLock` or a split of the slot |
| `awaken-sandbox-local` `NamespaceSandbox.memory_materializations` | invariant with existing refusals, but `Sandbox::handle()` cannot fail and dropping the materialization evidence from the handle is not a safe answer |
