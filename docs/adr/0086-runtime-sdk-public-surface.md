# ADR-0086: Runtime SDK Public Surface

- Status: Accepted
- Date: 2026-10-05 (accepted 2026-10-06; amended 2026-10-07, per-module tiers; amended 2026-10-08, item 7)
- Builds on: ADR-0059 neutral core and leaf evolution; ADR-0081 contract layering and
  ratchet guardrails (the depth baseline and the bounded-context ratchet).
- Leaves unchanged: ADR-0050 content capture and GDPR semantics; ADR-0051 tenancy
  semantics. This ADR decides where their types live, not what they mean.
- Amended: 2026-10-09 (names and references: `awaken-credential` removed; ADR-0051
  superseded by ADR-0063)

## Context

The Runtime SDK is the set of crates whose `[package.metadata.awaken]` says
`product = "sdk"`: the agent, tool and runtime contracts, the engine, the provider,
the reference store, observability, the leaf utilities (`awaken-time`,
`awaken-credential`, `awaken-outbound-http`, `awaken-mcp-wire`, `awaken-tool-pattern`),
the fence kernel `awaken-lease-contract`, and the nine `awaken-ext-*` extensions.
The bounded-context ratchet (`scripts/ci/bounded_context_baseline.json`) forbids an
SDK crate from depending on a Platform crate. Mechanical moves have already taken
the clock, the authority lease, the inference executor port, the ACP tool export and
the Memory head type out of the SDK's dependency closure. Three violations remain,
and all three are part of the SDK's public API rather than misplaced code:

| SDK crate | Platform dependency | What the SDK uses |
|---|---|---|
| `awaken-runtime-contract` | `awaken-credential-contract` | `pub use awaken_credential_contract::*` (82 items, 5.9k lines); `CredentialAccess` inside resolved model and remote routes |
| `awaken-runtime-contract` | `awaken-tenancy` | `ScopeId` in pinned credential routes; `ExecutionScopeRef` in `RuntimeContext` |
| `awaken-tool-contract` | `awaken-tenancy` | `ExecutionScopeRef` in `ToolCallContext` |

`awaken-runtime-contract` also carries seven modules that were first listed as
Platform modules. Moving them failed to compile: the engine or the snapshot types
read them. A reader count over the workspace (every crate except the defining one)
gives this picture:

| Module | SDK readers | Platform and app readers | Inside runtime-contract |
|---|---|---|---|
| `capture` | engine (`CaptureDecision`, `ContentCapture`, `ContentKind`); observability (`ContentRedactor`) | runtime-host, config-resolver, data-subject-application, captured-content-store, three apps | `runtime_context`, `data_subject` |
| `data_subject` | engine (`CaptureSink`, `DataSubjectId`, `Purpose`); builtin-tools (`Purpose`) | consent, resolution, erasure and organization privacy: seven Platform crates and three apps only | `RuntimeContext` and `RunActivation` hold `DataSubjectId` and `CaptureSink` |
| `credential` (own items) | ext-memory (receipts carried by an extraction request) | eleven Platform crates, two apps, scenario-host | none |
| `credential` (re-export) | builtin-tools web search (`CredentialAccess`, `CredentialRef`, `CredentialTarget`, `CredentialUsage`, `CredentialPurpose`, `ModelExposurePolicy`); ext-memory (`CredentialRef`, `PlaintextHolder`, realization kinds) | 34 Platform crates, four apps | resolved routes, provider candidates, agent bindings |
| `model_routing` | none | none | `resolved` only |
| `capability` | none | config-service, runtime-host, cli, control | none |
| `resolution` | none directly | agent-config, config-service | `ExecutableAgentSnapshot` holds a `ResolutionManifest` |
| `snapshot_builder` | runtime-examples | agent-config, config-service, admin-assistant, session-budget-contract | `resolved` |

Each module needs one of three answers: it is an SDK concept and stays; it is a
Platform concept and moves; or the SDK keeps an opaque handle and the Platform keeps
the meaning. Because each answer changes the SDK's public API, a decision is needed
before the moves.

## Decision

1. **`capture` stays in the SDK.** The engine enforces the capture decision on every
   message, and observability applies the redactor. Content capture policy is part of
   what the SDK does. Names are already neutral.

2. **`data_subject` is split.** The SDK keeps what the engine reads: `DataSubjectId`,
   `Purpose`, `CaptureSink` and `CaptureError`. These say whom captured content is
   about, why it was captured, and where it goes. Consent sources, subject resolution,
   erasure (`ContentEraser`, `ErasureReceipt`, `ErasureError`), organization privacy
   export and `NullResolver` move to a new Platform contract,
   `awaken-data-governance-contract` (`bounded_context = "governance"`). No SDK
   crate reads them.

3. **`capability` moves to the config context.** No SDK crate reads it. The host
   produces the runtime capability catalog and config-service validates against it,
   so it goes into a config-context contract crate. The target crate is chosen by the
   same reader count when the move is made.

4. **`model_routing` becomes crate-private.** Only `resolved` reads it, so it leaves
   the public API and stays where it is.

5. **`resolution` and `snapshot_builder` stay in the SDK.** `ExecutableAgentSnapshot`
   is the SDK's input. Its provenance (which inputs at which version produced it) and
   its typestate builder belong with it. SDK users build snapshots with the builder;
   the examples already do. `deserialize_max_steps` is a serde helper and becomes
   crate-private once its three Platform callers use the snapshot's own
   deserialization.

6. **The tenancy coordinate moves into the SDK as the same type.** `ScopeId` and
   `ExecutionScopeRef` are already opaque: serde-only newtypes over a string, with no
   behavior. They move to `awaken-agent-contract`. Both SDK contracts already depend
   on it. `awaken-tenancy` re-exports them, so Platform paths, type identity and wire
   shape do not change. `VerifiedExecutionScope`, `Authority` and every check that
   gives the coordinate meaning stay in `awaken-tenancy`. In the SDK, the coordinate
   is a label that the engine carries and never interprets.

7. **The credential vocabulary is split by reader.** First, the glob
   `pub use awaken_credential_contract::*` is replaced by an explicit list, so the
   SDK's surface becomes visible and reviewable. Then the reference vocabulary that
   SDK code reads moves into the SDK leaf `awaken-credential`: credential references,
   access, target, usage, purpose and exposure policy, plus the transitive closure
   their fields need. Two things stay in the Platform `awaken-credential-contract`:
   - custody, envelopes, descriptors, rollout and observation admission;
   - the attempt-level binding, delivery and realization receipts from
     runtime-contract's own `credential` module. They are read only by the Platform,
     except for one ext-memory field.
   The ext-memory extraction request carries those receipts as an opaque token, the
   way it already carries the Memory head. `awaken-credential-contract` re-exports
   the moved items, so Platform paths do not change.

The order follows risk. Items 4, 3 and 2 are internal or Platform-only moves. Item 6
moves two types without changing them. Item 7 is last, because the size of its
closure is known only after the explicit re-export list exists. Each step lowers
`bounded_context_baseline.json`, and the step that empties the `sdk-standalone` rule
removes the rule's entries from the baseline.

## Alternatives

- **Promote `awaken-credential-contract` and `awaken-tenancy` to the SDK.** This is
  the cheapest option, but it puts vault custody, sealed envelopes, rollout and
  authority verification into a library whose users have no vault and no tenants.
  The SDK would then know that the Platform exists.
- **Opaque handles everywhere.** The SDK would carry `CredentialHandle` and
  `ScopeLabel`, and the Platform would translate at the boundary. For the scope label
  this means writing the same type twice. For credentials it does not work: the
  provider and the web search tool read the access fields to authenticate, so an
  opaque handle would only move the same vocabulary behind a callback.
- **A separate SDK-side scope type with conversion.** This produces two types with one
  meaning and a conversion at every boundary, which is the duplication that
  ADR-0081's single-owner rule exists to prevent.

## Consequences

- The SDK depends on no Platform crate, and the bounded-context guard enforces this
  with an empty `sdk-standalone` baseline.
- The SDK public API shrinks. Consent, erasure, organization privacy and capability
  types disappear from it, and `model_routing` and the serde helper become private.
  The 82-item credential glob becomes an explicit list.
- `awaken-credential` grows from a three-item port crate to the credential reference
  vocabulary. Its `public-api` snapshot is added with the move.
- Platform import paths keep working through re-exports. Removing the re-exports is a
  later, separate cleanup.
- `awaken-agent-contract` gains two string newtypes, and `awaken-tenancy` gains a
  dependency on it in order to re-export them (Platform depending on SDK is the
  allowed direction). This adds one level to `awaken-tenancy`'s contract depth, and
  any growth is recorded in the ADR-0081 depth baseline with this ADR as the reason.
- Persisted data is unaffected. Every moved type keeps its serde shape, and the
  ext-memory receipt token serializes as the receipt it wraps.

## Amendment 2026-10-07: per-module tiers, values apart from traits

ADR-0088 states the SDK–Platform contract as one list and makes this ADR its single
source. The list needs every public SDK contract module placed in one tier, because
`awaken-runtime-contract` today holds three kinds of thing in one crate: values the
whole Platform reads (the Agent definition), traits only the Run execution side should
implement or drive (the engine SPI), and Platform vocabulary that items 2, 3, 4 and 7
above take off the public surface. From a dependency edge alone nobody can tell which
of the three a Platform crate uses.

**Tiers.** *Value* modules form the published language any context may read. *SPI*
modules are implemented or driven only by the Execution and Committed Truth contexts
(ADR-0088). *Leaves* means the items above take it off the public SDK surface.
*Mixed* modules hold values and SPI traits together; their traits count as SPI.

`awaken-runtime-contract`:

| Tier | Modules |
|---|---|
| Value | `resolved`, `snapshot` (with the private `snapshot_builder`, item 5), `resolution` (item 5), `agent_bindings`, `delegation`, `resilience` |
| SPI | `llm`, `plugin`, `execution`, `resume`, `pause`, `control`, `live_inbox`, `terminal`, `attempt_termination`, `compaction`, `boundary`, `metrics`, `runtime_context`, `resolver`, `activation`, `model_operation` |
| Mixed (values + SPI traits) | `capture` (values; `ContentRedactor`), `permission` (policy values; `ToolPermissionPolicy`, `PermissionGrantObserver`, `ToolGateHook`), `data_subject` (SDK part per item 2: `DataSubjectId`, `Purpose` values; `CaptureSink`) |
| Leaves | `capability` (item 3), `model_routing` (item 4), the governance part of `data_subject` (item 2: consent sources, resolvers, erasure, privacy export), the Platform part of `credential` (item 7) |

`model_operation` is the SDK's evidence-store port; its Platform implementation sits in
the credentials context today. Until that implementation is classified under ADR-0088,
it is the one recorded exception to the "Execution and Committed Truth only" rule.

`awaken-agent-contract` is Value except `thread::commit` (the commit coordinator and
operation traits), the reader traits in `thread::read` (`CheckpointReader`,
`RunRecoverySource`), and `stream::sink` / `stream::checkpoint`, which are SPI.
`awaken-tool-contract` is Value except the executor and discovery traits, which are SPI.

**Values apart from traits.** After items 1–7 land, value modules and the value half of
mixed modules move to `awaken-agent-contract` (with `awaken-tool-contract` for tool
values), and SPI traits move to `awaken-runtime-contract`. No crate is added; the two
crates already exist and their reader sets differ sharply (most Platform contexts read
values; two contexts implement the SPI). Item 5 still holds: the snapshot builder stays
next to the snapshot, so it moves with it.

**Re-exports.** `awaken-runtime-contract` re-exports `awaken-agent-contract` items, the
`awaken-tool-contract` modules, `awaken_tenancy::ExecutionScopeRef` and
`tokio_util::sync::CancellationToken` at its root. When the split happens it keeps only
an explicit list of re-exports for types that appear in its own trait signatures;
everything else is imported from the owning crate.

**Order.** The tier table applies now: ADR-0088's SPI guard reads it, ratcheted from the
current state. The value/trait move runs after items 1–7, when the closure of each
module is known.

## Amendment 2026-10-08: item 7 as the code requires it

Items 4, 3, 2 and 6 have landed, and `sdk-standalone` holds one entry:
`awaken-runtime-contract -> awaken-credential-contract`. A trial of item 7 on main (the
full change compiled workspace-wide) found that the decision text above does not match
the code in six places. This amendment replaces the text of item 7; items 1–6 stand.

How credentials flow today. The Agent snapshot's model routes carry `CredentialAccess`
(reference, target, usage, exposure policy; no secret). When a Worker claims an attempt,
the Platform commits the credential decisions as secret-free bindings and deliveries plus
a receipt recorder (`AttemptCredentialRealization`) and places them in
`RuntimeRunContext.credential_realization`. The engine carries the context and never reads
that field. The Platform's `LlmExecutor` reads it at the model request, materializes the
secret just in time and records a `CredentialRealizationReceipt`; ext-memory carries the
receipts of an extraction request. This works today only because the SDK depends on the
Platform crate (`pub use awaken_credential_contract::*`).

1. **Target.** The reference vocabulary moves into a `credential` module of
   `awaken-agent-contract`, not into `awaken-credential`. Every SDK reader already depends
   on `awaken-agent-contract`; `awaken-credential`'s three port items move with it and the
   crate is removed. `awaken-runtime-contract` re-exports the moved items from
   `awaken-agent-contract` (SDK to SDK), so their `awaken_runtime_contract::` paths keep
   working; `awaken-credential-contract` re-exports them too, so Platform paths keep working.
2. **Envelopes as values.** `CredentialAccess` holds `SealedCredentialEnvelopeRef` and
   `CredentialEnvelope` in its fields, so both move into the SDK as values (the closure
   item 7 already allowed). Every operation on an envelope — custody, sealing, opening,
   issuance, rollout, observation admission — stays in the Platform.
3. **Behavior that names Platform types becomes Platform extension traits.** `admit`,
   `validate_for_holder` / `validate_issuance` and `validate_usage` read Platform types,
   so they leave the SDK types' inherent impls and become extension traits in
   `awaken-credential-contract`. Call sites import the trait, including awaken-cloud
   `gateway.rs:220` and `credential-envelope` `lib.rs:405`.
4. **The engine carries the attempt realization as an opaque handle.**
   `RuntimeRunContext.credential_realization` becomes
   `Option<CredentialRealizationHandle>`, a named SDK type wrapping
   `Arc<dyn Any + Send + Sync>` that the engine carries and never interprets — the same
   rule as the scope label in item 6, and the same shape as the context's existing
   Platform ports (`model_request_gate`, `ownership`). The Platform recovers the typed value
   through exactly one function, `AttemptCredentialRealization::from_context`, which fails
   closed when the handle is absent or of another type, as a missing binding does today.
   A type parameter on `RuntimeRunContext` was rejected: it would spread through every
   SPI trait and extension signature, for a type SDK users never name.
5. **Attempt receipts move to the Platform contract.** The binding, delivery and
   realization types of runtime-contract's own `credential` module move into
   `awaken-credential-contract`, which then depends on `awaken-runtime-contract`
   (Platform to SDK, the allowed direction; the ADR-0081 depth growth is recorded with this
   amendment as the reason). The ext-memory extraction request carries the receipts as an
   opaque token that exposes only the two values ext-memory reads: the claim epoch (`u64`)
   and the `CandidateFingerprint` (which moves with the reference vocabulary).
6. **awaken-cloud paths change.** Items leaving runtime-contract's own module cannot keep
   an `awaken_runtime_contract::` path, because the SDK may not re-export a Platform crate.
   awaken-cloud updates them when it next bumps its pin: `gateway.rs` (141, 222, 244, 329),
   `awaken-model-access/src/lib.rs` (39–40), `tests/materialize.rs:1433`,
   `tests/materializer.rs` (22–52, 225).

Order: the explicit re-export list replaces the glob first; then the moves above as one
reviewable series; the series that empties `sdk-standalone` removes the rule's entry from
`bounded_context_baseline.json`.

## Amendment 2026-10-09: names and references

- `awaken-credential` → removed. The Context lists it as an SDK leaf, and the
  Consequences say it "grows from a three-item port crate". The 2026-10-08 amendment
  (item 1) moved the vocabulary and the three port items into the `credential` module
  of `awaken-agent-contract` instead (`crates/contract/awaken-agent-contract/src/lib.rs:7`).
  No public-API snapshot for `awaken-credential` is added; the moved items are covered
  by the `awaken-agent-contract` snapshot.
- Item 7 has landed: `awaken-runtime-contract` no longer depends on
  `awaken-credential-contract`, and the `sdk-standalone` baseline is empty
  (`scripts/ci/bounded_context_baseline.json:45`).
- "ADR-0051 tenancy semantics" (header) → ADR-0051 is Superseded by
  [ADR-0063](0063-resource-input-identity-configuration-pinning-and-lifecycle.md);
  this ADR leaves the tenancy semantics of ADR-0063 unchanged.


### 2026-10-14: Explicit SDK publication qualification

Publishable SDK members are selected from their existing `product = "sdk"` and
Cargo `publish` metadata, using the public-API selector shared by the snapshot
check and the explicit SDK qualifier. The current deep public paths remain part
of their reviewed snapshots; qualification does not reshape or hide them.

`check_sdk_consumer.py` retains its independent seven-task path by default.
`--qualify-package` separately requests standard Cargo package verification for
all publishable SDK members, default and all features, on
`x86_64-unknown-linux-gnu`. A registry dependency blocker fails the complete
selection; excluding the provider or skipping verification cannot qualify it.
The explicit `--target` argv needs its own actual receipt, rather than borrowing
an earlier host-default build.

`--qualify-semver --baseline-rev
50b3f3e22dbfe1d91dd6ee6fef8ee8dfc8e15940` compares both explicit feature subjects
on that Linux target against the first pre-release source contract. This
immutable baseline is an ancestor distinct from the tested HEAD, not a previous
published release. The existing tool manifest owns the pinned SemVer tool;
ordinary API snapshots do not require that optional tool, while an explicit
qualification refuses unknown or mismatching observations.

The quality workflow's dispatch selection is `none` by default, with explicit
`package` or `semver` choices. The canonical Native 13-group gate and its record
remain unchanged. SDK logs and archive hashes are evidence of their exact
source/feature/target subject. They do not prove registry publication, the full
artifact closure of the seven-task consumer, or cross-platform compatibility.
The controlled-IO test suite runs only under `--self-test`; real qualification
retains the ordinary lightweight admission-rule check.

### 2026-10-18: Reuse the published inference-speed value

`agent_bindings::InferenceSpeed` remains the current SDK authority for inference
speed. Dream and Managed model fields use that value directly and remove their
`DreamModelSpeed` and `ModelSpeed` mirrors and the identity speed translations.
They introduce no alias, forwarding export, new contract, or runtime mechanism.
Managed's geography and effort translations remain because their wire and
neutral forms differ. Dream's create and policy validation still reject `fast`
at their original pre-effect boundary, with the original error ordering.

This change preserves the complete SDK definition and reviewed deep public
path at SDK `1.0.0`. Moving that value into `awaken-agent-contract` now while
deleting the old SDK path would remove a path already present in the immutable
pre-release SemVer baseline. The eventual value/trait split above therefore
remains a formal SDK release change; this implementation does not bless a new
SDK surface or claim that a source migration is SemVer-compatible.

Both `awaken-session-dream-contract` and `awaken-dream-application` gain a direct
owner dependency on `awaken-runtime-contract`. The former gains the four pure
SDK contracts `awaken-runtime-contract`, `awaken-tool-contract`,
`awaken-agent-contract`, and `awaken-time` in its normal internal closure; its
contract depth grows from 1 to 3. The application already reaches those same
contracts, so its direct edge adds no transitive crates. No SDK engine,
provider, Platform implementation, or forbidden engine-SPI use is added.
The contract-depth ratchet records this particular reviewed growth through its
existing `--write-baseline --reason` path; no threshold or checker is weakened.

- Reason: remove two copies of one published value while preserving the frozen
  SDK `1.0.0` definition and public path, as permitted by the Value tier above.
- Approved by: the user's full v4 duplication-removal authorization and the
  Codex integration owner's bounded dependency review.
- Date: 2026-10-18.
- Remove the growth allowance when: the planned value/trait split moves the
  shared values to their final SDK owner under the formal release process;
  lower the measured Dream depth in that same change.

Omission, `null`, `standard`, `fast`, and invalid-literal behavior stay owned by
the existing model DTOs and SDK serde value. Tests keep the original validation,
wire, frozen-control, and recovery assertions. The original SDK snapshot check,
independent consumer, and explicit SemVer runner remain the release oracles;
ordinary compilation is not their substitute.
