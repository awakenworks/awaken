//! Core contract types and traits for the awaken agent framework.
//!
//! This crate defines the shared vocabulary used across all `awaken-*` crates:
//! data-model primitives (phases, effects, state), protocol traits (tools,
//! inference, events, lifecycle, storage), and utility types (cancellation,
//! threads, time). Runtime crates implement these traits; user code and
//! extension crates consume them.
//!
//! Most items are re-exported from the `awaken` facade crate. Import
//! `awaken::prelude::*` for the common subset, or use the sub-modules below
//! for precise imports.

#![allow(missing_docs)]

pub mod agent_spec_patch;
pub mod builtin_seed;
pub mod cancellation;
pub mod config_loader;
pub mod config_record;
pub mod config_validation;
pub mod contract;
mod error;
pub mod identity;
pub mod model;
pub mod periodic_refresh;
pub mod registry_spec;
pub mod secret;
pub mod skill_allowed_tools;
pub mod skill_spec;
pub mod skill_spec_patch;
pub mod state;
pub mod thread;
pub mod time;
pub mod tool_spec;
pub mod tool_spec_patch;

// ── time ──
pub use time::now_ms;

// ── error ──
pub use error::{StateError, UnknownKeyPolicy};

// ── model ──
pub use model::{
    EffectSpec, FailedScheduledActions, JsonValue, PendingScheduledActions, Phase,
    ScheduledActionSpec, TypedEffect,
};

// ── agent spec patch ──
pub use agent_spec_patch::{AgentSpecPatch, merge_agent_spec};

// ── tool spec patch ──
pub use tool_spec_patch::{ToolSpecPatch, merge_tool_spec};

// ── registry spec (AgentSpec, PluginConfigKey) ──
pub use registry_spec::{
    AgentSpec, HomeStrategy, McpRestartPolicy, McpServerSpec, McpTransportKind, Modalities,
    Modality, ModelPoolSpec, ModelSpec, PluginConfigKey, PoolMemberRole, PoolMemberSpec,
    PoolRoutingPolicy, PoolSwitchPolicy, ProviderSpec, StickyScope,
};

// ── skill spec ──
pub use skill_allowed_tools::{
    AllowedTool, AllowedToolParseError, is_skill_allowed_tool_pattern,
    parse_skill_allowed_tool_token, parse_skill_allowed_tools, validate_skill_allowed_tool_pattern,
};
pub use skill_spec::{
    PreparedSkillSpecs, SkillArgumentSpec, SkillSpec, SkillSpecContext, SkillSpecSink,
};
pub use skill_spec_patch::{SkillSpecPatch, merge_skill_spec};

// ── secret ──
pub use secret::RedactedString;

// ── state ──
pub use state::{
    KeyScope, MergeStrategy, MutationBatch, StateCommand, StateKey, StateKeyOptions, StateMap,
};
pub use state::{PersistedState, Snapshot};

// ── progress ──
pub use contract::progress::{
    ProgressStatus, TOOL_CALL_PROGRESS_ACTIVITY_TYPE, ToolCallProgressState,
};

// ── durable event sink ──
pub use contract::durable_event_sink::{
    AgentEventNormalizationContext, AgentEventNormalizer, DurableEventSink,
    NormalizedCanonicalEvent, RuntimeEventDurability, ScopedAgentEventNormalizer,
};

// ── commit coordinator ──
pub use contract::commit_coordinator::{
    CanonicalEventStager, CheckpointCommitOutcome, CheckpointCommitPlan, CommitCoordinator,
    CommitError, DiagnosticEvent, DiagnosticEventPublisher, EventPublishError,
    OutboxServerEventPublisher, ServerCanonicalEvent, ServerEventPublishOutcome,
    StagedCanonicalEvent, TransactionScopeId,
};

// ── canonical event store ──
pub use contract::event_store::{
    AppendOptions, AppendResult, CanonicalEvent, CanonicalEventDraft, CanonicalEventId,
    CanonicalEventKind, CanonicalEventStream, EventCursor, EventLookup, EventPage, EventReader,
    EventScope, EventScopeFamily, EventScopeIds, EventStore, EventStoreError, EventSubscriber,
    EventVisibility, EventWriter, FidelityClass, SubscribeHandle, SubscribeStart,
};

// ── protocol replay log ──
pub use contract::protocol_replay_log::{
    ProtocolReplayAppendResult, ProtocolReplayCursor, ProtocolReplayDraft, ProtocolReplayError,
    ProtocolReplayId, ProtocolReplayLog, ProtocolReplayLookup, ProtocolReplayPage,
    ProtocolReplayReader, ProtocolReplayRecord, ProtocolReplayRedactionState, ProtocolReplayWriter,
    ProtocolStreamKey, ScopedProtocolReplayLog, SourceEventCursor,
};

// ── outbox ──
pub use contract::outbox::{
    OUTBOX_LANE_CANONICAL, OUTBOX_LANE_PROTOCOL_REPLAY, OUTBOX_TARGET_A2A_WEBHOOK,
    OUTBOX_TARGET_PROTOCOL_FANOUT, OUTBOX_TARGET_PROTOCOL_PROJECTOR, OutboxEnqueueResult,
    OutboxError, OutboxMessage, OutboxMessageDraft, OutboxNackOutcome, OutboxStatus, OutboxStore,
    ScopedOutboxStore,
};

// ── mailbox ──
pub use contract::mailbox::{
    MailboxInterrupt, MailboxInterruptDetails, MailboxStore, RunDispatch, RunDispatchResult,
    RunDispatchStatus, ScopedMailboxStore,
};
pub use contract::registry_graph::{
    REGISTRY_KIND_AGENT, REGISTRY_KIND_MODEL, REGISTRY_KIND_MODEL_POOL,
    REGISTRY_KIND_PLUGIN_CONFIG, REGISTRY_KIND_PROVIDER, REGISTRY_KIND_SKILL, REGISTRY_KIND_TOOL,
    RegistryGraphValidationError, RegistryGraphValidationReport, RegistryGraphValidationRequest,
    RegistryGraphValidator, RegistryReferencePolicy, StandardRegistryGraphValidator,
    VersionSelector,
};
pub use contract::run::{
    RunActivationSnapshot, RunInput, RunInputSnapshot, RunIntent, RunKind, RunOptions,
    RunResolutionScope, RunTraceContext,
};
pub use contract::scope::{
    DEFAULT_SCOPE_ID, RequestSurface, ScopeContext, ScopeError, ScopeId, scoped_key, unscoped_key,
};
pub use contract::storage::{RunRequestOrigin, ScopedThreadRunStore};
pub use contract::versioned_registry::{
    ConfigRevisionRef, PinnedRegistryEntry, PinnedRegistryManifest, PublishOutcome,
    RegistryPublication, RegistryResourcePublish, RegistryRetentionPolicy, ScopedVersionedRegistry,
    TypedVersionedRegistry, VersionRef, VersionedRecord, VersionedRegistryError,
    VersionedRegistryRetention, VersionedRegistryStore, VersionedResourceState,
    canonical_registry_json_bytes, registry_content_hash,
};

// ── profile store ──
pub use contract::config_store::{
    ConfigChangeEvent, ConfigChangeKind, ConfigChangeNotifier, ConfigChangeSubscriber, ConfigStore,
    ScopedConfigStore,
};
pub use contract::profile_store::{ProfileEntry, ProfileKey, ProfileOwner, ProfileStore};

// ── shared state ──
pub use contract::shared_state::StateScope;

// ── tool schema ──
pub use contract::tool::TypedTool;
pub use contract::tool_schema::{generate_tool_schema, sanitize_for_llm, validate_against_schema};

// ── thread ──
pub use thread::{Thread, ThreadMetadata};

// ── tool spec ──
pub use tool_spec::ToolSpec;

// ── cancellation ──
pub use cancellation::{CancellationHandle, CancellationToken};

// ── periodic refresh ──
pub use periodic_refresh::PeriodicRefresher;

// ── audit log ──
pub use contract::audit_log::{AuditAction, AuditEvent};

// ── config record envelope ──
pub use config_record::{
    ConfigRecord, ConfigRecordError, ConfigRecordMerge, NoConfigPatch, RecordMeta, RecordSource,
    decode_config_record, effective_config_record, effective_visible_config_records,
    validate_config_record_overrides,
};
pub use config_validation::{
    AGENT_SPEC_PATCH_UNKNOWN_FIELD_POLICY, AGENT_SPEC_UNKNOWN_FIELD_POLICY, ConfigValidationError,
    MODEL_POOL_SPEC_UNKNOWN_FIELD_POLICY, MODEL_SPEC_UNKNOWN_FIELD_POLICY,
    PROVIDER_SPEC_UNKNOWN_FIELD_POLICY, SKILL_SPEC_UNKNOWN_FIELD_POLICY, UnknownFieldPolicy,
    validate_agent_spec, validate_agent_spec_patch, validate_config_record,
    validate_model_pool_spec, validate_model_pool_spec_struct, validate_model_spec,
    validate_model_spec_struct, validate_provider_spec, validate_skill_spec,
    validate_unique_model_ids,
};

// ── builtin seed ──
pub use builtin_seed::{BuiltinSeedSet, BuiltinSpec};
