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
| Value | `resolved`, `snapshot` (with the private `snapshot_builder`, item 5), `resolution` (item 5), `agent_bindings`, `delegation`, `resilience`, `model_inspection` (committed-prefix values only) |
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
  of `awaken-agent-contract` instead (`crates/sdk/contracts/awaken-agent-contract/src/lib.rs:7`).
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

`check_sdk_consumer.py` retains its independent eight-task path by default.
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
artifact closure of the eight-task consumer, or cross-platform compatibility.
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

### 2026-10-19: One SDK conversation binding

The SDK binding is `Conversation`, constructed by `Runtime::conversation`.
`EmbeddedSession` was a deprecated alias of this same type and `Runtime::session`
only forwarded to the same constructor. Removing these redundant entries and
migrating their callers gives the binding one public name and one constructor.

The execution, approval, committed outcome and recovery implementations are
unchanged. Two bindings to the same Snapshot, Thread, store and clock exercise
both starter directions, exact-identity recovery and terminal replay through
that one implementation. Owner-attached examples independently reject the old
type and method names and accept the canonical binding.

Both removed entries exist in the immutable SDK qualification baseline
`50b3f3e22dbfe1d91dd6ee6fef8ee8dfc8e15940`. Their removal is a real source API
break; deprecation does not waive the API, downstream-consumer or SemVer checks.
The Runtime package advances from `1.0.0` to `2.0.0`; the workspace default and
other packages keep their versions. Cargo owns the corresponding lock updates.
The public-API tool must report and review the two removed entries. The SDK
release qualifier must independently prove the baseline calls, reject each
removed entry, accept the migrated caller, and audit the actual SemVer findings
before publication. This decision does not alter a SemVer exception list or
relax another published path's compatibility obligation. It adds no execution
state, dependency, constructor, forwarding export or alternate recovery path.

### 2026-10-18: Execute the original consumer from package archives

The explicit `--qualify-artifact` mode uses that same qualifier, source identity
and command sequence owner. It packages every selected SDK member and the normal
workspace dependency closure required by the original eight-task consumer,
including its real filesystem restart adapter. Supplementary Platform archives
are test dependencies, not newly selected SDK members or a publication decision.

After standard Cargo package verification, the qualifier validates each archive's
hash, normalized members and exact clean-source VCS identity. A fresh independent
copy of the original consumer uses exact package versions and a supported
top-level Cargo [`[patch.crates-io]`](https://doc.rust-lang.org/cargo/reference/overriding-dependencies.html)
pointing only at those unpacked archives.
The complete resolved graph must contain no workspace dependency from the source
checkout; archive and copied-source bytes remain fixed through the original
eight tasks and sample. This is archive consumption, not a registry, a source
replacement, or the private Cargo package overlay API.

The default path consumer and both existing package/SemVer feature subjects keep
their original behavior. Artifact tasks exercise their actual minimal-feature
consumer graph on the explicit Linux target; they do not turn all packaged SDK
features into consumer features. Actual public-registry publication/consumption,
cross-platform execution and real-provider acceptance remain separate receipts.
The existing quality workflow exposes this mode as the explicit dispatch choice
`artifact`, after exact SHA binding and before dependency installation. Its
default remains `none`; it does not redefine the Native gate's thirteen groups.

### 2026-10-19: Explicit Provider and Builtin breaking release cohort

Decision: accepted after the original-source compile controls succeeded. The
same two consumers against the registry candidate failed with E0004 (the four
added exhaustive enum variants) and E0308 (the original Git Client argument).
The controls used source commit 7c58904f3c7961df24a36fbf50d4b7a4512aeed2;
the failing candidate source tree was 7265f150dfd7e2fb7482314aab1d53f47156e2f7.
Both compiler failures retained exit 101. This decision acknowledges those
breaks; it does not bless the current 1.0 candidate or qualify a 2.0 artifact.

The SDK selector still includes the same 22 published members. Only
`awaken-provider-genai` and `awaken-ext-builtin-tools` enter an explicit 2.0.0
release cohort. The other twenty package versions, the root workspace default
version, Runtime Contract's ninety historical facade paths and nine renamed paths,
and their reviewed public API snapshots remain unchanged by this decision.

Provider's original 1.0 source contract exposes the Git-pinned genai Client and
other genai types and reexports its exhaustive AdapterKind. A single registry
genai 0.7.0-rc.8 source changes that public dependency identity and adds four
AdapterKind variants. These are acknowledged source breaks. The original Git
Client argument and exhaustive old match remain immutable failing observations
against the new candidate, alongside successful original-source controls. The
new release uses one registry genai source in development and packaging; it adds
no second genai dependency, public adapter enum, type bridge, store or transport.
A migrated downstream selects the same registry Client and explicitly handles
Mistral, Requesty, ApiRoute and Heabsy. Its success proves the documented migration,
not compatibility with the old 1.0 source contract.

Builtin's public selected_hand_recovery_modes function was removed while Hand
membership projection was consolidated into the Platform Session Configuration
owner. This release acknowledges the removed 1.0 SDK path. Platform callers use
the existing Hand projection at that boundary; the SDK does not depend on that
Platform owner or recreate its algorithm. The four original Web policy root types
remain public paths to the one existing Web Policy SDK implementation, with their
original variants, fields and serialization. No duplicate Web DTO is introduced.

The first pre-release source baseline remains
50b3f3e22dbfe1d91dd6ee6fef8ee8dfc8e15940. Original 1.0 failure receipts are retained.
The original twenty-two-package default/all-feature SemVer qualifier runs against
that baseline and the explicit new package versions; a major-version allowance
is a versioned release observation, never a claim that old source now compiles.
Cross-crate rustdoc limitations and actual downstream compiler outcomes are
reported separately. API snapshots are checked without a blanket bless or
exclusion. Unexpected additional breaks block the cohort.

Clean-source standard package verification, actual archive consumer tasks and
sample, public registry publication/resolve, physical Provider behavior,
cross-platform behavior and the Native thirteen-group record keep their separate
evidence boundaries. Every qualified command binds its exact final clean source,
package versions, target, features and artifact hashes. Nothing in this amendment
authorizes a push or registry publication or changes the SDK member selector,
baseline, deny policy, ordinary consumer task set or full-gate acceptance.

### 2026-10-19: Bind the Runtime break to the existing release qualifier

The separate Conversation decision above advances `awaken-runtime` to `2.0.0`
for its real baseline API removals. Together with the earlier Provider/Builtin
release decision, the current release observation covers three `2.0.0` packages;
the other nineteen SDK package versions and the workspace default remain `1.0.0`.
The earlier two-package observations remain historical evidence of their exact
source, not evidence for this later Runtime change.

The same qualifier adds the two original Runtime source calls against `50b3`,
their two exact compiler refusals against the canonical source, and a successful
Conversation type/method migration. The original nine compile controls and their
fourteen core steps remain; each feature subject now has fourteen controls and
nineteen steps. All use the existing source, archive, metadata, compiler and
retained-byte receipt owner. No compatibility entry or second audit runner is
introduced.

The Runtime findings have not yet been observed or approved. Its forced-patch
report must have nonempty queries and exact reviewed findings; the former
Provider/Builtin approval cannot admit it. Missing or unknown Runtime evidence
keeps the same fail-closed result, even when the other packages are complete.
Both original twenty-two-package standard SemVer feature subjects, standard
package verification, consumer tasks/sample and real archive consumption still
require their own final clean-source evidence. This preparation neither qualifies
a Runtime artifact nor authorizes a registry publication.


### 2026-10-20: Version the original retry-policy argument break

The immutable first pre-release baseline `50b3f3e22dbfe1d91dd6ee6fef8ee8dfc8e15940`
already exposes `resilience::RetryPolicy::decide` with a concrete
`ModelExecutionEvidence` argument. The returned-response retry correction now
uses the canonical `ModelReentry` projection of phase, original effect scope
and execution evidence. These are distinct argument types; the original SDK
call is a source break, including through Runtime's same-type reexport.
Runtime's separate `2.0.0` decision cannot version the defining Contract package.

Retain the one corrected retry implementation and declare
`awaken-runtime-contract` explicitly `2.0.0`, with its workspace dependency
requirement updated to `2.0.0`. The workspace default and every unrelated package
version stay unchanged. The existing qualifier's declared release cohort now
includes Contract alongside Provider, Builtin and Runtime. The previous three
package and two package observations remain historical; no old receipt is
rewritten or claimed for this source. Cargo owns both lock updates on reception.

The same compile-only witness owner adds three designed observations: identical
old-decide input on the baseline, its current typed argument refusal, and the
canonical phase/scope/execution projection on current source. The original nine
controls/fourteen core steps and Runtime's fourteen controls/nineteen steps
remain explicitly checked subsets; the complete subject has seventeen controls
and twenty-two steps per existing default/all feature profile. Expected E0308
and raw101 describe a test design, not an executed result or SemVer finding.

The existing outer six actions, full twenty-two-package standard selection,
source/target/feature checks, raw output and primary failure retention, metadata
and archive ownership remain unchanged. Only the forced audit's declared
package selection grows; it must produce its own complete nonempty Contract
report. Runtime and Contract findings and approvals remain unestablished and
absent from the production approval data. Missing or unknown evidence continues
to fail closed through the same validator; no empty approval is inferred.

This declares the package source break; it does not qualify an artifact,
approve a report, authorize registry publication, alter snapshots blindly or
add a second retry classifier, old-decide wrapper, alias, runner or profile.
Actual baseline/current compiler controls, default/all SemVer reports, standard
package/archive consumption and the original release evidence are still required.

### 2026-10-20: Canonical Agent and Tool imports in the same Contract2 source cohort

Runtime Contract's ninety first-party forwarding leaves (Agent86, Tool4), nine
renamed leaves and fifty-one dependent local forwarding locations are real
public baseline paths. Remove this redundant Contract surface together, before
freezing the final Contract2 release subject. Callers name the existing Agent
or Tool defining owner directly; the hundred-and-thirty-six same-owner outlets
and third-party CancellationToken remain. No definition, identity minting,
codec, hash, credential policy, tool effect, persistence or retry law moves.
The forwarding-only credential module is removed, along with the two Ingress
Contract CandidateFingerprint forwarding outlets. Runtime's already-public
ToolCall facade names point directly to Tool, preserving those facade names.

Agent and Tool keep their actual existing versions. They are ordinary compiler
consumer dependencies, selected from each original side's authored metadata,
not additional major-cohort members. The one existing witness renderer and
metadata validator bind their source/version/features. Original nine/fourteen,
fourteen/nineteen and seventeen/twenty-two compiler-control subsets remain;
baseline full-route inventory, five isolated old-route shape controls and a
both-side canonical-owner positive extend the complete design to thirty cases
and thirty-five steps per default/all profile. Expected compiler refusals are
test design only; actual raw exits, findings and source receipts are unobserved
until the original executor records them. The four-package major COHORT,
original six outer actions and complete twenty-two standard SDK packages stay.

The existing exact approval data for Runtime and Contract remain absent pending
actual nonempty default/all forced-patch reports and review. This source change
makes earlier SDK17-only receipts insufficient for the later public API subject;
it does not predict findings, approve a report, qualify a package or authorize
registry publication. Both lock files and public snapshots are future outputs
of their original generators and must bind the final received source. Flow and
each Cloud consumer need a separate import/selector/pin migration and their own
actual build receipts; no downstream source or pin changes are made here.


### 2026-10-20: Record observed Runtime and Contract major findings

Pinned cargo-semver-checks 0.47 actually produced matching default/all forced-
patch observations for source `7df94e4654` against the original `50b3` baseline.
Both packages executed 222 nonempty checks. Runtime reports the removed
`Runtime::session`, `Runtime::with_max_consecutive_inference_failures` and
`EmbeddedSession`; Contract reports the added `Error:Admission` variant and the
removed forwarding-only `credential` module. These exact sets now join the
already reviewed Provider/Builtin findings in the existing qualifier's data.
The earlier unestablished observations above remain historical statements.

The complete SDK22 normal/build workspace source closure is unchanged through
`f5316e0dbc`. This source comparison supports review of the actual findings;
it does not relabel an old command as final-source execution. Original compiler
controls and fresh default/all reports, raw statuses, nonempty queries, complete
package accounting and archive receipts remain required by the same validator.
Missing or extra findings still refuse. No generic allowance, exception list,
compatibility wrapper, witness renderer or release executor is added.

The separately executed Agent `1.0.0` report found the added exhaustive-enum
variant `Disposition:StopRouting`. This four-package observation does not admit
that unversioned break or settle Agent/Contract public dependency identity.
Versions and release-cohort membership are unchanged here. Full22 diagnostics,
actual downstream interoperability, version decisions and the complete final-
source SDK package/SemVer/archive/registry qualification remain open. No push or
registry publication is authorized by this finding review.


### 2026-10-20: Close the public defining-type release boundary

Complete pinned 0.47 forced-patch diagnostics on clean `f5316e0dbc` actually
checked all twenty-two selected SDK packages, with 222 nonempty queries each,
under both default/all profiles against the original `50b3` baseline. The exact
findings agree across profiles. Agent adds the exhaustive `Disposition` variant
`StopRouting`; keeping Agent at `1.0.0` would leave that source break unversioned.
The original baseline-positive, current E0004 and migrated-positive controls
already execute that distinction. Preserve the corrected routing behavior.

Public dependency identity is a separate source obligation. Actual default
compiler inputs use the real `OtelMetricsRecorder`, `PiiRedactor` and
`CompactPlugin`, with the accepted Contract fixed to the original immutable
Git source. All three baseline compositions compile; all three current SDK
compositions with accepted Contract1 refuse with E0277 at the original value
flow; all three current Contract2 compositions compile. Their real Cargo
metadata contains both exact Contract identities in the negative graph and
only the canonical identity in each positive graph. These are bounded actual
compiler observations, not Provider, registry or final-release qualification.

The existing API/source review finds Agent-defined public values or traits in
fourteen other selected SDK packages. Observability separately exposes Contract
traits without an Agent payload. These are actual fields, arguments, results
and trait implementations; an indirect private dependency alone is insufficient.
Keeping their old package versions would allow a supposedly compatible upgrade
to replace the defining identity seen by a downstream caller. Identical API text
and primitive payload fields do not make Contract1 and Contract2 the same trait.

Declare Agent and Tool explicitly `2.0.0`; retain the separately justified
Builtin, Provider, Runtime and RuntimeContract `2.0.0` decisions. BackgroundTask,
Compact, Goal, Memory, Permission, Observability and StoreInmem declare `2.0.0`
for their public defining-type change. MCP, Skills and StateMachine retain their
pre-1.0 status and use the incompatible `0.2.0` boundary. Update the existing
workspace dependency requirements to these authored versions. The six leaves
LeaseContract, McpWire, OutboundHttp, Time, ToolPattern and WebPolicy retain their
actual versions. The workspace default and Platform versions remain unchanged.
No production dependency edge, type definition, retry classifier, conversion,
forwarding API or compatibility implementation is added.

The one existing release-cohort and exact-observation data describe this
sixteen-package boundary. The forced reports' empty sets on public carriers
are recorded tool observations, never assertions of cross-version compatibility.
Unknown/additional findings still refuse; new versions require their own fresh
nonempty default/all reports, original compiler controls, standard twenty-two
package observations and archive/consumer receipts. Historical receipts keep
their original source. No blanket approval or release mode is introduced.

A package may now have both a release role and a defining-owner role. The same
SDK input owner renders their union once across manifest, producer metadata,
consumer metadata and retained receipt. Neither role suppresses the other:
Agent/Tool still require their exact direct normal dependencies. Attached U1-U3
controlled rules exercise disjoint/overlapping roles on both source sides and
feature profiles, including transitively reached but dev-only direct edges.
The baseline version tests retain real workspace1 inheritance and the three
literal0.1 declarations; claimed metadata cannot replace authored identity.

Cargo owns both root and independent-consumer lock updates. Tests, normal
commit checks, final clean-source API/SemVer/package/artifact consumption,
registry resolution, downstream migrations and the Native thirteen-group
qualification remain distinct required evidence. This source/version decision
neither publishes a registry artifact nor authorizes a push.


### 2026-10-31: One versioning rule for every SDK crate

The release decisions above were taken crate by crate. From now on one rule
versions every published SDK crate, enforced per commit by
`scripts/ci/check_sdk_semver.py`: the committed public-API snapshot is compared
with the snapshot at the SDK baseline (`[workspace.metadata.awaken]
sdk_semver_baseline`, today `50b3f3e22d`), and the crate version must carry the
bump that difference requires in Cargo's compatibility positions, with a
`CHANGELOG.md` section for it. Exposing another SDK crate's types makes that
crate's major bump this crate's major bump, which reproduces the sixteen-package
cohort above. Versions are relative to the baseline, so a pending major absorbs
later breaks until it is released. The release qualifier keeps its role for what
snapshot text cannot show. The table, the version map and the decisions on the
`LiveInbox` lock, the three `ThreadUsage` forwarding paths and the SDK
optional-role shapes are in [SDK versioning](../design/sdk-versioning.md).

## Amendment — 2026-10-23: inspect original Session configuration and model attempts

With `include_inspection=true`, the existing execution-evidence read includes a safe configuration projection
of the same frozen Session baseline and its exact published executable snapshot.
Environment revision, network, packages, resource limits, plugin configuration,
context policy and delegation limits retain their original owner; no inspection
store or current-draft fallback is introduced. A missing publication pin or an
unavailable historical snapshot is reported explicitly, never replaced by the
current Agent. Secret environment values, inline mount bytes and credential
capabilities are excluded. Plugin configuration uses the shared sensitive-field
scrubber; typed credential bindings remain references only.

Run model routes come from `ActiveModelOperation::recorded` over the same committed
Thread snapshot already used for execution evidence. These are recorded attempts,
including uncertain entry, rather than proof of a successful provider call. The
Session's selected model is never relabeled as each Run's actual model.

Reused unchanged: Session repository, immutable executable catalog, canonical
model-operation decoder and capture policy. Modified: the existing read projection
and Console inspector. New: only safe transient projection values. Reads are
Workspace-fenced and perform no realization, dispatch, credential resolution or
retry. Missing history stays absent; malformed committed operations fail the read.

The content-capture endpoint reports today's policy decision, not a historical
request receipt. Existing captured telemetry contains scrubbed message text but
lacks a trustworthy Session/Run association and a complete provider request. The
inspector must report final request content as unavailable instead of reconstructing
it from today's draft, the conversation transcript or that policy decision.

The [frozen Session baseline](0066-session-service-binding-and-realization.md)
retains its original owner. `model_inspection` is Value language: its pure read
accepts an already committed command prefix and exposes no engine handle,
evidence-store port, allocator, mutation, reentry decision or execution permission.
Session readers use this boundary instead of importing the engine SPI. The
original decoder remains the only model-journal interpretation.
