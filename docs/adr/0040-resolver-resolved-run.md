# ADR-0040: `Resolver` + `ResolvedRun` + Typed Backend Capabilities

- **Status**: 🚧 Proposed
- **Date**: 2026-05-22
- **Depends on**: ADR-0010, ADR-0035, ADR-0039
- **Updates**: ADR-0010 D4/D8/D9, ADR-0011 D2/D6, ADR-0035 runtime pinning
- **Breaking**: yes (0.6.0)

## Context

Three resolver-shaped concepts overlap today:

```text
AgentResolver::resolve(id) -> ResolvedAgent
ExecutionResolver::resolve_execution(id) -> ResolvedExecution::{Local | NonLocal}
RunRegistryManifestResolver::manifest_for_run(agent_id) -> PinnedRegistryManifest
```

Backend capability reporting is also mixed. Some dimensions are typed
(`cancellation`, `continuation`, `waits`, `transcript`, `output`) while
`decisions`, `overrides`, and `frontend_tools` are bools and unsupported
feature errors are returned as `Vec<&'static str>`.

The result is a fragmented setup path: runtime code resolves an agent,
asks another resolver whether execution is local or remote, asks mailbox
which registry version to pin, and then performs a separate capability
check.

## Decision

Collapse run materialisation into one async `Resolver` that returns a
`ResolvedRunPlan`. The plan is the complete execution plan, including live
execution handles. Capability requirements are derived from the activation
by runtime code, not supplied manually by external callers.

### D1: Single async `Resolver` trait

`Resolver`, `ResolutionRequest`, `ResolvedRunPlan`, `ResolvedRun`, and
`BackendProfile` live in `awaken-runtime`. Server code already depends on
runtime and calls this surface at dispatch time; `awaken-contract` only owns
the serializable run and event contract types.

```rust
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(
        &self,
        request: ResolutionRequest,
    ) -> Result<ResolvedRunPlan, ResolveError>;
}
```

Spec-only reads are not a second method on `Resolver`. They use a separate
read-side trait or existing registry interface:

```rust
#[async_trait]
pub trait AgentSpecLookup: Send + Sync {
    async fn resolve_spec(&self, agent_id: &str) -> Result<AgentSpec, ResolveError>;
}
```

This keeps the execution-planning trait from gaining a method whose
"must not materialize execution state" rule cannot be enforced by the
signature. Implementations that can provide both traits may do so, but
callers choose the narrower trait when they only need a spec.

### D2: `ResolutionRequest` is a projection of `RunActivation`

`Resolver` does not receive the full activation. Runtime builds a
projection that contains identity, registry scope, and run features from
which backend requirements are derived:

```rust
#[derive(Debug, Clone)]
pub struct ResolutionRequest {
    pub target: ResolutionTarget,
    pub resolution_scope: RunResolutionScope,
    pub overrides: Option<InferenceOverride>,
    pub frontend_tools: Vec<ToolDescriptor>,
    pub features: RunFeatureSet,
}

#[derive(Debug, Clone, Default)]
pub struct RunFeatureSet {
    pub has_seeded_decisions: bool,
    pub has_live_decision_channel: bool,
    pub has_overrides: bool,
    pub has_frontend_tools: bool,
    pub is_human_resume: bool,
    pub is_continuation: bool,
    pub requested_persistence: PersistenceRequirement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionPolicy {
    PersistentServer,
    LiveOnlyEmbedded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PersistenceRequirement {
    NotRequired,
    CheckpointRequired,
}

impl Default for PersistenceRequirement {
    fn default() -> Self { Self::NotRequired }
}

impl From<ResolutionPolicy> for PersistenceRequirement {
    fn from(policy: ResolutionPolicy) -> Self {
        match policy {
            ResolutionPolicy::PersistentServer => Self::CheckpointRequired,
            ResolutionPolicy::LiveOnlyEmbedded => Self::NotRequired,
        }
    }
}

impl RunFeatureSet {
    pub fn from_activation(activation: &RunActivation, policy: ResolutionPolicy) -> Self {
        Self {
            has_seeded_decisions: !activation.control.seeded_decisions.is_empty(),
            has_live_decision_channel: activation.control.decision_rx.is_some(),
            has_overrides: activation.options.overrides.is_some(),
            has_frontend_tools: !activation.options.frontend_tools.is_empty(),
            is_human_resume: matches!(&activation.intent.kind, RunKind::HitlResume { .. }),
            is_continuation: matches!(
                &activation.intent.kind,
                RunKind::ContinuationFromRun { .. },
            ),
            requested_persistence: PersistenceRequirement::from(policy),
        }
    }
}

pub enum ResolutionTarget {
    Root { agent_id: String, thread_id: String },
    Delegate { agent_id: String, parent_run: RunIdentity, persistence: DelegatePersistence },
    Handoff { agent_id: String, from_agent: String, transcript_ref: HandoffTranscriptRef },
}
```

The mapping from activation to features is fixed:

| Feature | Source |
|---|---|
| `has_seeded_decisions` | `!activation.control.seeded_decisions.is_empty()` |
| `has_live_decision_channel` | `activation.control.decision_rx.is_some()` |
| `has_overrides` | `activation.options.overrides.is_some()` |
| `has_frontend_tools` | `!activation.options.frontend_tools.is_empty()` |
| `is_human_resume` | `RunKind::HitlResume` |
| `is_continuation` | `RunKind::ContinuationFromRun` |
| `requested_persistence` | `ResolutionPolicy::PersistentServer` → `CheckpointRequired`; `LiveOnlyEmbedded` → `NotRequired` |

`BackendRequirements` is derived from `RunFeatureSet` by a deterministic
runtime function:

```rust
impl BackendRequirements {
    pub fn from_features(features: &RunFeatureSet) -> Self { /* ... */ }
}
```

`PersistenceRequirement::CheckpointRequired` maps to
`Some(PersistenceCapability::Checkpoint)`; profile checking treats stronger
profiles such as `CrossSession` as satisfying that requirement.
`NotRequired` maps to no persistence requirement.

External callers do not hand-write requirements before resolution.
`ResolutionPolicy::PersistentServer` is selected by persistent
mailbox/server submit and dispatch paths; `ResolutionPolicy::LiveOnlyEmbedded`
is selected by embedded live-only calls.
A resolver may use the derived requirements to choose among candidate
backends. After resolution, the runtime recomputes the same requirements and
checks `plan.backend_profile.check(&requirements)` as a defensive invariant
before execution. A mismatch is reported as a resolve/capability error and
no execution side effect has started.

For persistent submits, `RunResolutionScope::Live` means "materialize a
pinned registry snapshot now"; `RunResolutionScope::Pinned(manifest)` means
"resolve against this exact pinned snapshot." The durable queue stores only
that pinned snapshot. Dispatch resolves again from the pinned scope to
recreate live handles.

### D3: Replayability is encoded in the returned plan

A resolved run is either replayable or live-only:

```rust
pub enum ResolvedRunPlan {
    Replayable(ResolvedRun<ReplayableScope>),
    LiveOnly(ResolvedRun<LiveOnlyScope>),
}

pub struct ReplayableScope {
    pub manifest: PinnedRegistryManifest,
}

pub struct LiveOnlyScope;

pub struct ResolvedRun<S> {
    pub agent_spec: AgentSpec,
    pub role: ExecutionRole,
    pub execution: ExecutionPlan,
    pub model: ResolvedModel,
    pub tools: Vec<ResolvedTool>,
    pub overrides: Option<InferenceOverride>,
    pub backend_profile: BackendProfile,
    pub requirements: BackendRequirements,
    pub scope: S,
}
```

Server/mailbox persistent execution accepts only
`ResolvedRun<ReplayableScope>`. Embedded non-persistent execution may use
`LiveOnly`. This replaces prose-only "server runs must be pinned" rules
with a type boundary.

### D4: `ResolvedRun` contains live execution handles

`ResolvedRun` is not just serializable metadata. It includes everything the
runtime needs to dispatch:

```rust
pub enum ExecutionPlan {
    Local {
        env: ExecutionEnv,
        llm_executor: Arc<dyn LlmExecutor>,
        tool_executor: Arc<dyn ToolExecutor>,
        context_summarizer: Option<Arc<dyn ContextSummarizer>>,
        background_manager: Option<Arc<BackgroundTaskManager>>,
        stream_checkpoint_store: Option<Arc<dyn StreamCheckpointStore>>,
    },
    Remote {
        backend_id: String,
        endpoint: RemoteEndpoint,
        backend: Arc<dyn ExecutionBackend>,
    },
}
```

This preserves the execution information carried today by
`ResolvedAgent` and `ResolvedBackendAgent`. Downstream execution no longer
reassembles a plan from agent spec, backend registry, and runtime globals.

### D5: Typed backend profile and multi-mismatch decisions

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendProfile {
    pub cancellation: CancellationCapability,
    pub continuation: ContinuationCapability,
    pub decisions: DecisionCapability,
    pub overrides: OverrideCapability,
    pub frontend_tools: FrontendToolCapability,
    pub persistence: PersistenceCapability,
    pub waits: WaitCapability,
    pub transcript: TranscriptCapability,
    pub output: OutputCapability,
}

pub enum DecisionCapability { None, LiveOnly, DurableResume, LiveAndDurable }
pub enum OverrideCapability { None, InferenceParams, ModelAndParams }
pub enum FrontendToolCapability { None, DescriptorsOnly, Executable }
pub enum PersistenceCapability { Ephemeral, Checkpoint, CrossSession }

pub struct BackendRequirements {
    pub cancellation: Option<CancellationCapability>,
    pub continuation: Option<ContinuationCapability>,
    pub decisions: Option<DecisionCapability>,
    pub overrides: Option<OverrideCapability>,
    pub frontend_tools: Option<FrontendToolCapability>,
    pub persistence: Option<PersistenceCapability>,
    pub waits: Option<WaitCapability>,
    pub transcript: Option<TranscriptCapability>,
    pub output: Option<OutputCapability>,
}

pub enum CapabilityDecision {
    Supported,
    Unsupported(Vec<CapabilityMismatch>),
}
```

The new profile keeps the existing typed dimensions (`waits`,
`transcript`, `output`) while replacing bool-only dimensions with typed
capabilities. `BackendProfile::check(&BackendRequirements)` returns all
mismatches in one result.

`BackendProfile::full_local()` has a fixed contract:

| Capability | `full_local()` value |
|---|---|
| `cancellation` | `CooperativeToken` |
| `continuation` | `InProcessState` |
| `decisions` | `LiveAndDurable` |
| `overrides` | `ModelAndParams` |
| `frontend_tools` | `Executable` |
| `persistence` | `Checkpoint` |
| `waits` | `InputAndAuth` |
| `transcript` | `FullTranscript` |
| `output` | `TextAndArtifacts` |

### D6: 0.6.0 removals and migration adapters

0.6.0 removes these internal bridges:

- `LocalExecutionResolver`.
- `RunRegistryManifestResolver`.
- `ResolvedExecution::{Local, NonLocal}`.
- `BackendCapabilities::unsupported_*_features() -> Vec<&'static str>`.
- `BackendCapabilities` bool fields `decisions`, `overrides`, and
  `frontend_tools`.

`ExecutionResolver` cannot remain source-compatible because its return type
is deleted. It is removed with `ResolvedExecution`.

`AgentResolver` is user-facing enough to get a narrow adapter for embedded
or test code:

```rust
pub struct LocalRegistryResolver<R: AgentResolver> { /* ... */ }

#[async_trait]
impl<R: AgentResolver + 'static> Resolver for LocalRegistryResolver<R> {
    async fn resolve(&self, req: ResolutionRequest) -> Result<ResolvedRunPlan, ResolveError> {
        let ResolutionTarget::Root { agent_id, .. } = &req.target else {
            return Err(ResolveError::UnsupportedTarget(
                "legacy AgentResolver adapter supports root local resolution only".into(),
            ));
        };
        if matches!(req.resolution_scope, RunResolutionScope::Pinned(_)) {
            return Err(ResolveError::UnsupportedPersistence(
                "legacy AgentResolver adapter cannot materialize pinned registry scopes".into(),
            ));
        }
        let agent = self.inner.resolve(agent_id)?;
        Ok(ResolvedRunPlan::LiveOnly(ResolvedRun {
            agent_spec: (*agent.spec).clone(),
            role: ExecutionRole::Root,
            execution: ExecutionPlan::from_resolved_agent(agent),
            backend_profile: BackendProfile::full_local(),
            requirements: BackendRequirements::from_features(&req.features),
            scope: LiveOnlyScope,
            /* ... */
        }))
    }
}
```

The adapter is explicitly live-only. Persistent server/mailbox paths reject
it because they require `ResolvedRun<ReplayableScope>`.

### D7: Nested resolution for delegate / handoff

Root resolution happens at the dispatch entry (mailbox/server submit and
dispatch, embedded `run_*` call). Sub-runs spawned mid-execution
(`ResolutionTarget::Delegate`, `ResolutionTarget::Handoff`) cannot be
enumerated upfront, so `AgentRuntime` keeps an `Arc<dyn Resolver>` and calls
it during execution:

```rust
pub struct AgentRuntime {
    resolver: Arc<dyn Resolver>,
    /* other fields */
}
```

Nested resolution is **not** a back-door around the dispatch-entry contract.
The scope of a sub-run is constrained by the root's scope:

| Root plan scope | Allowed sub-run plan scope | On `LiveOnly` return |
|---|---|---|
| `ReplayableScope` | `ReplayableScope` only | runtime fails the parent run with `ResolveError::NestedScopeMismatch` |
| `LiveOnlyScope` | either | accept and continue |

The runtime constructs the nested `ResolutionRequest` with
`resolution_scope` inherited from the root (the root's
`PinnedRegistryManifest` for replayable runs, `Live` for live-only) and the
appropriate `ResolutionTarget::{Delegate, Handoff}`. Sub-run
`RunFeatureSet` is derived from the sub-run's own `RunActivation` (not the
root's), via the same `RunFeatureSet::from_activation` function.

`Resolver::resolve` itself does not know whether a request is root or
nested — that distinction lives in `ResolutionTarget`. Resolvers that
cannot honor `Delegate` or `Handoff` return
`ResolveError::UnsupportedTarget`, which the runtime surfaces as a normal
tool/spawn error rather than as a top-level dispatch failure.

This keeps the dispatch-entry rule ("persistent paths reject `LiveOnly`")
intact: the runtime enforces it for sub-runs the same way the dispatcher
enforces it for the root, using the same type boundary.

## Migration

| 0.5.x | 0.6.0 |
|---|---|
| `impl AgentResolver for MyResolver` | implement `Resolver`, or wrap with `LocalRegistryResolver` for live-only embedded use |
| `impl ExecutionResolver for MyResolver` | implement `Resolver` directly |
| `LocalExecutionResolver::new(agent_resolver)` | `LocalRegistryResolver::new(agent_resolver)` for live-only root runs |
| `RunRegistryManifestResolver` | manifest materialization inside `Resolver::resolve` |
| `resolver.resolve_execution("agent")?` | `resolver.resolve(activation.resolution_request()).await?` |
| `caps.unsupported_root_features(req)` | derive `BackendRequirements`, then `profile.check(&requirements)` |
| `caps.decisions = true` | `profile.decisions = DecisionCapability::LiveAndDurable` |

## Risks

- `ResolvedRun` is richer than `ResolvedExecution`; test construction needs
  builders.
- Legacy `AgentResolver` fixtures that need persistence must migrate to
  real `Resolver` implementations because the adapter cannot pin registries.

## Test Plan

1. `RunFeatureSet::from_activation` derives every feature from the owned
   activation plus `ResolutionPolicy`; callers cannot hand-write
   persistence requirements.
2. `BackendRequirements::from_features` derives decisions, overrides,
   frontend tools, continuation, waits, transcript, output, and persistence
   requirements from activation features.
3. `BackendProfile::full_local()` matches the fixed capability table and
   `BackendProfile::check` returns every mismatch, not only the first.
4. A server/mailbox path rejects `ResolvedRunPlan::LiveOnly` before
   persistence or execution.
5. `LocalRegistryResolver` succeeds only for root live local runs and
   fails for delegate, handoff, remote, or pinned requests.
6. `ResolvedRun<ReplayableScope>` contains the live handles needed by local
   and remote execution without a second registry lookup.
7. Runtime validation fails before execution side effects when a resolver
   returns a plan whose profile does not satisfy derived requirements.
8. Persistent submit with `RunResolutionScope::Live` materializes a pinned
   registry snapshot, and dispatch with `RunResolutionScope::Pinned`
   resolves against exactly that pinned snapshot.
9. A delegate/handoff spawned from a `ReplayableScope` parent run that
   resolves to `LiveOnly` fails the parent run with
   `ResolveError::NestedScopeMismatch`; the same spawn from a `LiveOnly`
   parent run is accepted.

## Non-Goals

- Per-tool capability negotiation. The current requirements cover run-level
  capabilities; per-tool flags can be added without changing the resolver
  boundary.
- Streaming resolution. `resolve` is a one-shot setup call.
