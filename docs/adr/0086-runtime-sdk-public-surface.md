# ADR-0086: Runtime SDK Public Surface

- Status: Proposed
- Date: 2026-10-05
- Builds on: ADR-0059 neutral core and leaf evolution; ADR-0081 contract layering and
  ratchet guardrails (the depth baseline and the bounded-context ratchet).
- Leaves unchanged: ADR-0050 content capture and GDPR semantics; ADR-0051 tenancy
  semantics. This ADR decides where their types live, not what they mean.

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
