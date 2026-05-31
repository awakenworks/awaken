# ADR-0035: Published Versioned Registry and Runtime Pinning

- **Status**: Accepted
- **Date**: 2026-05-21
- **Depends on**: ADR-0010, ADR-0014, ADR-0018, ADR-0019, ADR-0025, ADR-0028, ADR-0034

## Context

Awaken already has a serializable registry model (`AgentSpec`,
`ModelSpec`, `ProviderSpec`, `ToolSpec`, `SkillSpec`), a `ConfigStore`
for admin-managed JSON records, and a runtime `RegistrySnapshot` with a
monotonic snapshot version. The server can rebuild a `RegistrySet` from
`ConfigStore` and replace the runtime registry handle.

That model is enough for live config updates, but it does not provide immutable
published runtime-config versions. The `ConfigRecord.meta.revision` field is an
editing-time compare-and-set value, not a runtime version. The
`RegistrySnapshot::version` field is a whole-snapshot cache key, not a stable
per-resource version. Audit-log restore can reapply an older payload, but it is
not a versioned registry with current pointers, content hashes, archived
runtime-config resources, atomic publication records, or run-pinned resolution
ids.

Production runtime behavior needs four separate concepts:

```text
ConfigStore              editing / draft / CAS revision
VersionedRegistryStore   immutable published per-runtime-config versions
RegistryPublication      atomic published graph snapshot
RegistrySnapshot         in-memory runtime cache version
```

Runs also need repeatable resolution after resume, handoff, and delegation.
The runtime should not query PostgreSQL or any other async store while resolving
agents and tools on the hot path.

## Non-goals

- Introducing a direct SQL-backed `AgentSpecRegistry` as the runtime lookup
  model.
- Making `AgentSpecRegistry` async.
- Putting workspace, organization, billing, quota, or hosted tenant isolation
  into `awaken-runtime`.
- Encoding protocol-specific fields, such as Anthropic AgentVersion payloads,
  into `AgentSpec`, `VersionedRegistryStore`, or runtime loops.
- Replacing `ThreadRunStore`, `MailboxStore`, `EventStore`,
  `ProtocolReplayLog`, or `OutboxStore`.
- Implementing downstream protocol surfaces, hosted tenant business policy,
  credential lifecycle products, hosted sandbox fleets, resource data planes,
  approval workflows, or billing-driven retention policy.

## Decisions

### D1: Separate editing revision, published version, publication, and snapshot version

`ConfigStore` remains the admin editing store. It owns CRUD, draft state,
seeded records, optimistic concurrency through `meta.revision`, audit restore,
and UI editing workflows.

`VersionedRegistryStore` is a new published-state store. It owns immutable
per-runtime-config versions, current pointers, rollback-by-copy, archive
markers, content-addressed no-op detection, and historical reads for pinned
runs.

`RegistryPublication` is the atomic published graph boundary. It records one
publish operation's selected runtime-config versions across agents, skills,
tools, models, providers, and plugin config. A current run materializer
resolves the latest publication, not a set of independently observed resource
current pointers.

`RegistrySnapshot` remains the runtime in-memory cache. It is a synchronous
`RegistrySet` plus a monotonic runtime cache version. It is not a persistence
layer and does not replace per-resource versions or publication records.

### D2: Add a generic store plus typed wrappers in `awaken-server-contract`

The published registry applies to published runtime configuration resources,
not only agents:

- `agent`
- `skill`
- `tool`
- `model`
- `provider`
- `plugin_config`

The underlying store is kind-discriminated and serde-json based so one backend
can atomically publish mixed resource kinds:

```rust
pub struct VersionRef {
    pub kind: String,
    pub id: String,
    pub version: u64,
}

pub struct VersionedRecord<T> {
    pub kind: String,
    pub id: String,
    pub version: u64,
    pub content_hash: String,
    pub value_schema_version: u32,
    pub value: T,
    pub canonical_json_bytes: Vec<u8>,
    pub created_at_ms: u64,
    pub metadata: serde_json::Value,
}

pub enum PublishOutcome<T> {
    Created(VersionedRecord<T>),
    Noop(VersionedRecord<T>),
}

pub struct VersionedResourceState {
    pub scope_id: String,
    pub kind: String,
    pub id: String,
    pub current_version: Option<u64>,
    pub archived_at_ms: Option<u64>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub metadata: serde_json::Value,
}

pub struct ConfigRevisionRef {
    pub namespace: String,
    pub id: String,
    pub revision: u64,
}

pub struct RegistryPublication {
    pub publication_id: String,
    pub scope_id: String,
    pub snapshot_version: u64,
    pub entries: Vec<VersionRef>,
    pub source_config_revisions: Vec<ConfigRevisionRef>,
    pub created_by: Option<String>,
    pub created_at_ms: u64,
    pub metadata: serde_json::Value,
}
```

The storage trait is asynchronous because it belongs to server/store layers:

```rust
#[async_trait::async_trait]
pub trait VersionedRegistryStore: Send + Sync {
    async fn resource_state(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedResourceState>, VersionedRegistryError>;

    async fn current(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Option<VersionedRecord<serde_json::Value>>, VersionedRegistryError>;

    async fn get(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        version: u64,
    ) -> Result<Option<VersionedRecord<serde_json::Value>>, VersionedRegistryError>;

    async fn list_versions(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<Vec<VersionedRecord<serde_json::Value>>, VersionedRegistryError>;

    async fn publish_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        value: serde_json::Value,
        value_schema_version: u32,
        metadata: serde_json::Value,
    ) -> Result<PublishOutcome<serde_json::Value>, VersionedRegistryError>;

    async fn rollback_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
        to_version: u64,
        metadata: serde_json::Value,
    ) -> Result<VersionedRecord<serde_json::Value>, VersionedRegistryError>;

    async fn archive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError>;

    async fn unarchive_resource(
        &self,
        scope_id: &str,
        kind: &str,
        id: &str,
    ) -> Result<(), VersionedRegistryError>;

    async fn create_publication(
        &self,
        scope_id: &str,
        publication_id: &str,
        entries: Vec<VersionRef>,
        source_config_revisions: Vec<ConfigRevisionRef>,
        created_by: Option<String>,
        metadata: serde_json::Value,
    ) -> Result<RegistryPublication, VersionedRegistryError>;

    async fn latest_publication(
        &self,
        scope_id: &str,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError>;

    async fn get_publication(
        &self,
        scope_id: &str,
        snapshot_version: u64,
    ) -> Result<Option<RegistryPublication>, VersionedRegistryError>;

    async fn pinned_manifest_for_publication(
        &self,
        scope_id: &str,
        snapshot_version: u64,
    ) -> Result<Option<PinnedRegistryManifest>, VersionedRegistryError>;

    async fn latest_pinned_manifest(
        &self,
        scope_id: &str,
    ) -> Result<Option<PinnedRegistryManifest>, VersionedRegistryError>;
}
```

A typed wrapper binds `scope_id`, `kind`, and serde codecs for consumers that
work with concrete Awaken specs:

```rust
pub struct TypedVersionedRegistry<T> {
    pub store: std::sync::Arc<dyn VersionedRegistryStore>,
    pub scope_id: String,
    pub kind: String,
    pub _phantom: std::marker::PhantomData<T>,
}
```

`TypedVersionedRegistry<T>` delegates to `VersionedRegistryStore`, handles
serde, resource-specific schema migration, and returns `VersionedRecord<T>`.
Mixed-kind publish, publication history, and backend conformance tests use the
untyped store directly. `create_publication` records a committed graph snapshot
from already-published version references and validates that each referenced
version exists in the same `scope_id`; higher-level server publish orchestration
uses this store operation after publishing the selected resource versions.
The default pinned-manifest helpers load the publication entries and attach the
stored content hashes needed by the server-owned pinned manifest referenced by
`RunRecord.resolution_id`.

`awaken-runtime` must not depend on these traits for run execution. Runtime uses
a materialized synchronous registry snapshot.

### D2a: Published values carry schema evolution rules

Published values must remain readable across Awaken upgrades. Every versioned
record carries a `value_schema_version`; resource-specific codecs own any
migration from older stored shapes into the current Rust type.

For Awaken-owned published types such as `AgentSpec`, `SkillSpec`,
`ModelSpec`, and `ProviderSpec`:

- adding fields requires `#[serde(default)]` or an equivalent defaulting codec,
- deleting or renaming stored fields is forbidden unless a migration codec remains
  available,
- write-time validation may reject unknown fields, but read-time deserialization
  of historical published values must be backward compatible,
- incompatible schema changes require a new `value_schema_version` and an
  explicit migration path, and
- store and wrapper `get` operations must surface an incompatible-schema error
  instead of silently returning `None` or drifting to the current version.

This rule is required because pinned runs may resume from historical published
bytes long after the process binary has been upgraded.

### D2b: Published registries and pinned manifests are not secret stores

Published runtime configuration values and server-owned pinned manifests must
not contain plaintext credentials or resolved secret material. `RedactedString`
is safe for logs and debug output, but its JSON representation is still
plaintext; it is not sufficient for immutable published registry storage.

This ADR does not introduce a generic `SecretRef`, `SecretResolver`, vault,
credential broker, or secret-lint subsystem. Awaken's contract-level invariant
is narrower: published registry values, publication metadata, pinned manifests,
EventStore rows, and ProtocolReplayLog rows must not persist resolved plaintext
secrets.

If an Awaken-owned schema must name a credential before constructing a model or
provider backend, it may carry a schema-owned opaque credential reference, such
as:

```rust
pub struct OpaqueCredentialRef {
    pub id: String,
    pub kind: Option<String>,
}
```

That reference is only a stable identifier. It is not a secret resolver contract
and does not define a vault backend, KMS policy, OAuth lifecycle, credential
sharing model, rotation workflow, hosted credential ACL, or audit UI. Resolving
the reference into secret material belongs to the downstream provider factory,
protocol adapter, deployment integration, or hosted product layer. Resolved
secret values must not be written back into `VersionedRegistryStore`,
`RegistryPublication`, pinned manifests, EventStore, or
ProtocolReplayLog.

Tool credentials, MCP OAuth tokens, GitHub clone tokens, file fetch tokens,
memory mount tokens, sandbox capabilities, image pull credentials, webhook HMAC
keys, and product credential resources are downstream concerns. They must not be
modeled as Awaken registry secrets.

Generic store implementations must not hard-code heuristic field-name scanning
such as `*_token`, `*_key`, `client_secret`, `password`, or `*_credential` as a
framework contract. Product-specific publish validators may enforce stricter
linting for their own DTOs and credential schemas before values reach the
published registry.

### D3: Publish uses canonical JSON and content hash no-op detection

Publishing converts the resource through its schema-specific canonical encoder,
serializes the resulting JSON to canonical UTF-8 bytes, and computes
`content_hash` as `sha256:<lowercase-hex>` over those bytes plus the explicit
`value_schema_version` envelope. The hash input may include opaque credential
reference identifiers when an Awaken-owned schema contains them, but it never
includes resolved plaintext secret material.

Awaken uses a stable canonical JSON profile:

- object keys are sorted lexicographically using the JSON string order defined by
  RFC 8785 JSON Canonicalization Scheme;
- strings are emitted as UTF-8 JSON strings without Unicode normalization;
- integers are preserved exactly;
- non-finite floats are rejected; finite numbers follow the RFC 8785 JSON number
  serialization rules;
- the resource-specific canonical encoder normalizes missing optional/default
  fields and explicit optional `null` values to the same canonical omission;
  required `null` values remain distinct;
- default-valued additive fields must not perturb the canonical bytes for older
  published values; and
- metadata, creation timestamps, version numbers, and publication identifiers are
  not part of `content_hash`.

Store implementations persist the canonical bytes produced by publish. JSONB may
be stored as an auxiliary query/debug representation, but deserializing and
re-serializing JSONB must not be the source of truth for hash verification.

No-op detection compares the new hash only to the current version's hash for the
same `(scope_id, kind, id)`. A matching current hash returns
`PublishOutcome::Noop` and does not create a new version. A hash that matches an
older historical version is still publishable as a new monotonic version when
rollback-by-copy or publication semantics require it.

Conformance tests must verify that memory, file, and PostgreSQL stores compute
identical hashes for canonical-equivalent fixtures.

### D4: Rollback publishes a new version instead of moving the pointer back

Rollback does not set `current_version` to an older version. It copies the
selected historical value and publishes that value as the next monotonic version
with metadata such as:

```json
{ "restored_from": 3 }
```

This keeps versions append-only and makes every current pointer advance
monotonically. Rollback may create a new version with the same `content_hash` as
a historical version. Historical `content_hash` values therefore must not be
unique within a resource.

A server rollback operation may target either one resource or a
`RegistryPublication`. Publication rollback copies every selected entry into a
new publication transaction and records the source publication in metadata.

### D5: Archive marks resources without deleting published history

Archive sets an archived marker on the resource record and keeps all historical
versions available. Archive state is per resource, not per version, and is
exposed through `VersionedResourceState`.

`current` may still return the current version of an archived resource for
administrative and explicit-version use. Callers that need active-only behavior
must call `resource_state` and check `archived_at_ms`.

Default lifecycle semantics are:

- latest-publication and current-version materialization reject archived
  resources;
- explicit version and pinned-manifest materialization may load archived
  historical versions;
- `publish_resource` rejects an archived resource and must not implicitly
  unarchive it;
- `rollback_resource` rejects an archived resource and must not implicitly
  unarchive it;
- `archive_resource` is idempotent and does not move the current pointer;
- `unarchive_resource` is an explicit administrative action that clears
  `archived_at_ms` without creating a new version; and
- a server restore flow that wants to republish an archived resource must make
  the unarchive step explicit in audit metadata.

Archive never physically deletes version rows and never invalidates a retained
`RegistryPublication` or retained `RunRecord.resolution_id` reference.

### D6: Store implementations live in `awaken-stores`

`awaken-stores` owns memory, file, and PostgreSQL implementations of
`VersionedRegistryStore`.

The default PostgreSQL shape uses generic resource, version, and publication
tables:

```sql
awaken_registry_resources (
  scope_id          text not null default 'default',
  kind              text not null,
  id                text not null,
  current_version   bigint,
  archived_at_ms    bigint,
  created_at_ms     bigint not null,
  updated_at_ms     bigint not null,
  metadata_json      jsonb not null default '{}',
  primary key (scope_id, kind, id)
);

awaken_registry_versions (
  scope_id              text not null default 'default',
  kind                  text not null,
  id                    text not null,
  version               bigint not null,
  content_hash          text not null,
  value_schema_version  int not null,
  canonical_value_json  text not null,
  value_json            jsonb not null,
  metadata_json         jsonb not null default '{}',
  created_at_ms         bigint not null,
  primary key (scope_id, kind, id, version)
);

create index awaken_registry_versions_hash_idx
  on awaken_registry_versions(scope_id, kind, id, content_hash);

awaken_registry_publications (
  scope_id                      text not null default 'default',
  snapshot_version              bigint not null,
  publication_id                text not null,
  source_config_revisions_json  jsonb not null default '[]',
  created_by                    text,
  metadata_json                 jsonb not null default '{}',
  created_at_ms                 bigint not null,
  primary key (scope_id, snapshot_version),
  unique (scope_id, publication_id)
);

awaken_registry_publication_entries (
  scope_id          text not null default 'default',
  snapshot_version  bigint not null,
  kind              text not null,
  id                text not null,
  version           bigint not null,
  content_hash      text not null,
  primary key (scope_id, snapshot_version, kind, id),
  foreign key (scope_id, snapshot_version)
    references awaken_registry_publications(scope_id, snapshot_version),
  foreign key (scope_id, kind, id, version)
    references awaken_registry_versions(scope_id, kind, id, version)
);
```

`content_hash` is intentionally indexed but not unique. No-op detection is a
pointer comparison against the current version, not a historical uniqueness
constraint.

All resource version rows, resource current-pointer updates, publication rows,
and publication entry rows for one publish operation are written in a single
store transaction. A latest publication becomes visible only after that
transaction commits.

`scope_id` is a store/server partition key. OSS and local deployments use
`scope_id = 'default'`. Hosted deployments map it to a workspace or equivalent
tenant boundary. `awaken-runtime` does not interpret `scope_id`.

### D7: Runtime resolution stays synchronous and snapshot-based

`AgentSpecRegistry` remains synchronous:

```rust
fn get_agent(&self, id: &str) -> Option<AgentSpec>;
fn agent_ids(&self) -> Vec<String>;
```

Runtime handoff, delegate resolution, and tool resolution must use an in-memory
snapshot. They must not await a database read.

`awaken-runtime` adds a pinned registry implementation:

```text
PinnedAgentSpecRegistry
  - agent_id -> AgentSpec
  - agent_id -> version metadata
  - implements AgentSpecRegistry
```

The existing `MapAgentSpecRegistry` remains useful for unpinned and test
registries.

### D8: Server materializes frozen registry sets from publications or explicit pins

`awaken-server` owns materialization from published versions into runtime
registries. A `FrozenRegistryMaterializer` loads a root agent and every
reachable published runtime-config resource needed by Awaken runtime resolution:

1. root agent,
2. delegate agents,
3. referenced skills,
4. model profiles,
5. provider profiles,
6. plugin config entries that participate in runtime resolution.

The output is both:

- a typed `PinnedRegistryManifest`, and
- a frozen `RegistrySet` backed by synchronous registries.

Server code persists an opaque `resolution_id` on `RunRecord` and
`RunActivationSnapshot` before runtime execution starts. The server owns the
manifest content behind that id; `awaken-runtime` receives only the frozen
`RegistrySet` and carries `RegistryResolutionScope::Pinned(String)` for
re-resolution.

The materializer accepts one of these policies:

- latest publication in a scope,
- an explicit version for the root resource,
- an explicit publication snapshot version,
- an existing `RunRecord.resolution_id` value that the server resolves to a
  pinned registry graph.

For latest-publication policy, the materializer resolves the latest
`RegistryPublication` and uses that publication's entries as the candidate graph.
It must not read independent per-resource current pointers in a way that can mix
versions from different publish transactions. For explicit root-version policy,
the materializer validates and freezes the reachable graph from that exact root.
For existing-resolution policy, it reloads exactly the manifest entries
referenced by the stored id.

Before producing a frozen registry set, the materializer invokes the standard
registry graph validator described in D11a. A materializer must not silently skip
missing, archived, cross-scope, or cyclic references.

### D9: `RunRecord.resolution_id` references server-owned registry pins

Registry pinning is stored on durable run records as an opaque
`resolution_id`, not as a serialized manifest payload. This keeps runtime and
store contracts simple: Awaken has one server-owned pinned registry payload
shape (`PinnedRegistryManifest`), but the runtime/store path references it by
id. This ADR must not introduce a generic
`RunManifest { kind, payload }` or `RunManifestStore`.

```rust
pub struct RunRecord {
    pub resolution_id: Option<String>,
}

/// Server-owned payload referenced by `resolution_id`.
pub struct PinnedRegistryManifest {
    pub publication_id: Option<String>,
    pub registry_snapshot_version: Option<u64>,
    pub entries: Vec<PinnedRegistryEntry>,
}

pub struct PinnedRegistryEntry {
    pub kind: String,
    pub id: String,
    pub version: u64,
    pub content_hash: String,
}
```

When a manifest is produced from a `RegistryPublication`,
`registry_snapshot_version` is the publication `snapshot_version` and
`publication_id` records the publication identity. For explicitly pinned graphs,
these fields may be absent while the entry list remains authoritative.

Entries in `PinnedRegistryManifest` contain only published runtime-config
versions needed by Awaken runtime resolution:

```text
agent
skill
tool
model
provider
plugin_config
```

It does not contain session resources, files, memory stores, git repositories,
vault credential lifecycle objects, image digests, mount plans, sandbox bindings,
runtime tiers, workspace quota, billing data, or hosted tenancy policy. Those are
downstream/session-manifest or hosted-layer concerns.

The default storage shape follows the `ThreadRunStore` backend's `RunRecord`
persistence. Backends persist `resolution_id` with the run row. The server owns
the storage and lookup of the manifest payload behind that id; it must not
expose a generic `(run_id, kind, payload)` manifest table for this ADR. Backends
that can transactionally create runs must persist the `RunRecord` and
`resolution_id` together or expose an equivalent all-or-nothing server
operation.

Run start writes `RunRecord.resolution_id` before execution starts. Resume reads
that field, materializes from the exact versions referenced by the id, and never
falls back to the current published version. Delegate and handoff resolution
must resolve only runtime-config resources present in the decoded pinned registry
graph; missing entries fail resolution instead of reading the current published
version.

`PinnedRegistryEntry.content_hash` is an integrity check, not only audit
metadata. Resume and manifest-based materialization reload each version's stored
canonical bytes, recompute the hash using D3, and fail with an integrity error
if the stored hash differs from the manifest entry. This requires stores to keep
the canonical bytes written by publish and not rely on serde round-tripping as
the hash source.

Existing stored runs with `resolution_id = None` are treated as legacy records.
New server-created runs must write `RunRecord.resolution_id` before execution
starts.

### D10: Agent IDs remain stable and version-free

`AgentSpec.id` stays as the logical resource id:

```text
agent_xxx
```

The version is carried by the decoded `PinnedRegistryManifest`, not by appending
a suffix to the runtime id. Server/materializer inputs may use current,
publication, or explicit `VersionRef` selectors, but protocol-specific API
selector syntax is owned by protocol adapters and does not enter this ADR.

### D11: Config publish and version publish are separate server actions

Admin CRUD writes `ConfigStore` and bumps `meta.revision`.

Publishing loads `ConfigStore`, validates the full graph, publishes changed
resources into `VersionedRegistryStore`, writes a `RegistryPublication`, builds a
frozen `RegistrySet`, and calls `RegistryHandle::replace`.

The resource version rows and the publication row must commit atomically. A
run-start materializer using current policy observes the latest committed
publication. It must never capture a manifest made from partially updated
resource current pointers.

Restore from audit log remains valid as an editing-store operation. Registry
rollback is separate: it copies a previous published resource version or every
entry in a previous publication into a new published version/publication.

`RegistryHandle::replace` is a hot-swap for future resolution only. It affects
new runs, admin previews, and unpinned current-version materialization. It does
not mutate an active run's `PinnedAgentSpecRegistry`, stored
`RunRecord.resolution_id`, or already-resolved tool/delegate set. Resume
materializes from the stored resolution id, not from the latest current
registry. This prevents admin publish from changing a run mid-flight.

### D11a: Registry graph validation is a shared contract

Publish and materialization use the same registry graph validator so
`awaken-server`, protocol adapters, and hosted products do not implement
conflicting reference semantics.

The contract shape is:

```rust
pub enum VersionSelector {
    LatestPublication { scope_id: String },
    Publication { scope_id: String, snapshot_version: u64 },
    Exact { scope_id: String, kind: String, id: String, version: u64 },
    // A server-owned manifest referenced by `RunRecord.resolution_id`.
    Manifest { scope_id: String, manifest: PinnedRegistryManifest },
}

pub struct RegistryGraphValidationRequest {
    pub root: VersionSelector,
    pub reference_policy: RegistryReferencePolicy,
}

pub enum RegistryReferencePolicy {
    SameScopeOnly,
}

pub struct RegistryGraphValidationReport {
    pub entries: Vec<PinnedRegistryEntry>,
}

#[async_trait::async_trait]
pub trait RegistryGraphValidator: Send + Sync {
    async fn validate(
        &self,
        request: RegistryGraphValidationRequest,
    ) -> Result<RegistryGraphValidationReport, RegistryGraphValidationError>;
}
```

Standard validation errors are stable contract variants:

```rust
pub enum RegistryGraphValidationError {
    MissingResource { kind: String, id: String },
    MissingVersion { kind: String, id: String, version: u64 },
    ArchivedReference { kind: String, id: String, version: Option<u64> },
    ContentHashMismatch {
        kind: String,
        id: String,
        version: u64,
        expected: String,
        actual: String,
    },
    CycleDetected { path: Vec<VersionRef> },
    InvalidReference { kind: String, id: String, reason: String },
    Backend(String),
}
```

The standard validator follows references from agents to delegate agents and
model profiles, and from model profiles to provider profiles. Explicitly pinned
`skill`, `tool`, and `plugin_config` entries are verified when present, but the
core validator does not infer product-specific skill/plugin references that are
not represented by Awaken runtime fields. Every reference resolves within the
same `scope_id` as the selected publication, exact selector, or decoded registry payload.

The validator rejects references that are missing, archived for new
current-version materialization, outside the allowed scope policy, hash-mismatched,
or cyclic. Explicit pinned historical references may load archived resources,
matching D5.

Principal ACL, quota, billing, rate limits, and protocol-specific field checks
remain outside the core validator. Server or hosted layers run those checks
before calling publish or materialization.

### D12: Multi-agent dispatch stays in the existing delegation model

Awaken keeps the current delegation architecture:

- `AgentSpec.delegates`,
- delegate tools resolved during registry resolution,
- `AgentTool`,
- `ExecutionBackend`,
- local delegate execution,
- A2A remote delegate execution,
- parent run, thread, and tool-call lineage.

No `MultiagentCoordinator` is introduced by this ADR. The added requirement is
version enforcement through the pinned manifest referenced by
`RunRecord.resolution_id`.

### D13: Permission hard-deny is independent of registry pinning

Regex and glob permission matching are already part of the permission rule
model and remain outside the registry-versioning problem.

This ADR does not require a new permission behavior to implement published
registry versions, publications, pinned manifests, or materialization. If
Awaken needs a system-level hard prohibition that cannot be overridden by later
rules, that change should land as a separate permission decision covering
evaluator precedence, tool-list filtering, preview APIs, rule serialization,
and skill elevation logic.

Actor, workspace, and role-based policy resolution does not enter the core
evaluator. A server-side resolver may map principal context to a
`PermissionRuleset`, and the evaluator remains a pure rule calculator.

### D14: PostgreSQL checkpoint and mailbox stores are `awaken-stores` concerns

`StreamCheckpointStore` belongs to `awaken-runtime-contract`; PostgreSQL
persistence belongs in `awaken-stores`. The PostgreSQL implementation uses a
run-keyed upsert table:

```sql
awaken_stream_checkpoints (
  scope_id          text not null default 'default',
  run_id            text not null,
  thread_id         text not null,
  upstream_model    text not null,
  checkpoint_json   jsonb not null,
  updated_at_ms     bigint not null,
  primary key (scope_id, run_id)
);
```

`MailboxStore` PostgreSQL persistence also belongs in `awaken-stores`. It must
preserve the durable dispatch lifecycle, lease semantics, retry counters,
interrupt epoch, dedupe behavior, and multi-worker claim safety. PostgreSQL
claim queries use row locking with `FOR UPDATE SKIP LOCKED`.

Redis can be added later as another store backend. It is not required for the
runtime contract because NATS already covers real-time distributed dispatch.

### D15: Outbox remains the cross-process event delivery primitive

`OutboxStore` is defined by ADR-0034 and remains separate from `MailboxStore`.
Mailbox dispatches deliver run work and control inputs. Outbox rows deliver
notifications that canonical events or protocol replay rows are ready for
independent consumers.

ADR-0034 owns the reusable `OutboxRelay` library. A ready-to-run
`awaken-outbox-relay` binary may wrap that library for generic lanes, but
downstream products own lane policy, webhook payloads, HMAC, and retry business
policy. Registry publish, materialization, and run pinning may enqueue outbox
rows when they need cross-process notification, but the relay does not change
registry versioning or run-pinning contracts and must not understand downstream
webhook semantics.

### D16: Sandbox lifecycle stays downstream

Sandbox execution, lifecycle, resource mounts, image selection, runtime tier,
warm pools, and deployment drivers are outside this ADR. They are not part of
`awaken-runtime` and are not encoded in server-owned pinned manifests.

Awaken keeps only the generic `Tool` / `ToolRegistry` invocation boundary. A
concrete tool implementation may call a downstream sandbox service, but sandbox
binding, resource mounting, credential injection, and deployment policy remain in
the protocol adapter or hosted product layer.

### D17: Protocol adapters do not pollute core contracts

Anthropic-compatible APIs, AI SDK, A2A, ACP, MCP, and future protocols belong
in protocol adapter crates or `awaken-server/src/protocols/*`.

Protocol adapters may translate protocol-specific agent selectors into a current,
publication, or explicit `VersionRef` materializer input before runtime starts.
This ADR does not define Anthropic request DTOs, JSON selector shapes, public
version fields, beta headers, public event names, or public error schema.

Protocol adapters may also map protocol permission policy payloads to
`PermissionRuleset` and protocol outcome fields to generic Awaken eval/outcome
metadata. Those fields do not enter `AgentSpec`, `VersionedRegistryStore`,
`RegistrySnapshot`, server-owned pinned manifests, or runtime loop code.

### D18: Hosted tenancy stays above runtime

Hosted concepts such as workspace, organization, user, quota, billing, and
tenant isolation stay in `awaken-server` or hosted product code.

`awaken-runtime` may carry opaque metadata such as `Thread.resource_id`, run
metadata, and event metadata, but it does not enforce tenant policy.

Server and store layers enforce hosted isolation by applying the principal's
scope to every hosted query, using `scope_id` or an equivalent partition key.

### D19: Registry retention preserves pinned manifests and publications

Version rows are immutable and retained by default. Store implementations may
support physical purge, but purge is subject to registry retention policy and
must never delete a version referenced by any retained
`RunRecord.resolution_id` or retained `RegistryPublication`.

A retention policy is evaluated per scope and kind. It may combine rules such as:

- keep the current version of every resource,
- keep every version referenced by retained publications,
- keep every version referenced by retained pinned manifests,
- keep the last `N` historical versions per resource, and
- keep versions newer than a configured age.

`awaken-server-contract` should expose a purge planning surface rather than deleting
blindly:

```rust
pub struct RegistryRetentionPolicy {
    pub keep_last_versions: Option<u64>,
    pub keep_younger_than_ms: Option<u64>,
}

pub trait VersionedRegistryRetention {
    async fn purge_eligible_versions(
        &self,
        scope_id: &str,
        policy: RegistryRetentionPolicy,
        dry_run: bool,
    ) -> Result<Vec<VersionRef>, VersionedRegistryError>;
}
```

Pinned-manifest retention follows `RunRecord` and thread retention. Registry
stores use retained `RunRecord.resolution_id` values as protection roots while
planning version purge. Archive is not a purge operation and leaves history
intact.

### D20: Downstream products own protocol, hosted, workflow, and data-plane semantics

This ADR keeps Awaken as a generic runtime and server substrate. Awaken owns
stable contracts, storage primitives, materialization, integrity checks, and
extension hooks. Downstream protocol adapters and hosted products own their
public API shape, tenant policy, resource data plane, credential lifecycle, and
deployment-specific control loops.

Awaken framework code may expose generic hooks or opaque metadata for these
concerns, but it must not encode their product semantics:

| Area | Awaken provides | Downstream owns |
|---|---|---|
| Protocol APIs | `VersionRef`, `PinnedRegistryManifest`, `ProtocolReplayLog`, generic projector/fanout substrate | Anthropic or Managed Agents DTOs, routes, beta headers, public event names, public IDs, `Last-Event-ID` contract, and public error schema |
| Hosted tenancy | `scope_id`, optional principal/auth hooks, metadata, same-scope defaults, access hook interfaces | workspace, organization, user, team, quota, billing, rate limit, tenant entitlement, sharing policy, and hosted administration UI |
| ACL and policy resolution | `RegistryReferencePolicy`, `RegistryReferenceAccessHook`, pure permission evaluator inputs | role inheritance, team membership, public/private resource policy, cross-workspace sharing rules, approval UX, and policy editor behavior |
| Secrets | the invariant that published registry values and pinned manifests contain no plaintext secrets; optional schema-owned opaque credential references for model/provider backend construction | secret resolvers, vault schema, KMS policy, OAuth grant/refresh, GitHub tokens, MCP credentials, tool credentials, sandbox credential injection, rotation workflow, sharing policy, product-specific linting, and credential audit UI |
| Resource data plane | optional opaque runtime-config references only when needed by runtime tools | memory-store contents, file blobs, git checkout contents, datasets, vector indexes, object storage, resource binding, and artifact lifecycle |
| Sandbox/resource execution | `Tool` / `ToolRegistry` invocation boundary only; no sandbox or resource binding data in `PinnedRegistryManifest` | sandbox lifecycle, resource mounts, credential injection, Kubernetes fleets, warm pools, Cilium policy, Karpenter/autoscaling, image warmers, runtime classes, tenant network isolation, mount plans, runtime tiers, and hosted sandbox quota |
| Publication workflow | `RegistryPublication`, `source_config_revisions`, `created_by`, and `metadata` | draft review, approval chains, staging or production promotion, release notes, rollout policy, and change-request workflow |
| Resource kinds | standard runtime config kinds: agent, skill, tool, model, provider, plugin_config; extension kinds must remain runtime config | product-specific schemas such as environments, memory stores, vault credentials, dreams, outcome evaluations, sessions, workspace files, sandbox bindings, or git credential products |
| Provider routing | model/provider profiles and optional server hooks | cost routing by plan, regional compliance routing, tenant provider quotas, vendor contract policy, and billing-tier fallback strategy |
| Observability | trace hooks, generic span attributes, content hashes, and correlation IDs | hosted dashboards, billing analytics, usage reports, service-level reports, and tenant audit reports |
| Retention | purge planning APIs and protection of retained publications/pinned manifests | exact retention duration, legal hold, plan-based limits, billing-driven purge, and per-tenant version quotas |
| Outcome and evaluation | generic run outcomes, eval hooks, or optional evaluator traits | Anthropic outcome evaluation fields, dream resources, official evaluation product schemas, and managed-agent completion criteria |

When a downstream product needs an extension registry object to participate in
Awaken's generic machinery, that object must still be runtime configuration.
Product data-plane, session, resource binding, and sandbox payloads stay outside
server-owned pinned manifests and are interpreted only by the owning adapter or
hosted layer.

## Consequences

- Runtime resolution remains fast and synchronous.
- Published config can be rolled back without losing monotonic version history.
- Current run materialization observes atomic publication snapshots instead of
  partially updated resource pointers.
- Run resume, handoff, and delegation can be made repeatable across config
  changes and verified against stored content hashes.
- Store implementations carry the complexity of immutable version persistence,
  canonical bytes, publication history, and retention planning, while
  `awaken-runtime` remains database-agnostic.
- Protocol-specific compatibility layers can evolve without adding foreign
  fields to core contracts.
- Hosted deployments gain a clear boundary for scope enforcement without
  embedding workspace semantics into runtime crates.
- Product-specific API, tenant, workflow, credential, and data-plane behavior
  remains outside Awaken framework code while still being able to use generic
  hooks and pinned manifests.

## Rejected alternatives

### Direct SQL AgentSpecRegistry

Rejected. It would put async database reads on the runtime resolution path,
weaken active-run consistency, and only solve agents while leaving skills,
tools, models, providers, and plugin config without a common
published-version model.

### Reusing `ConfigRecord.meta.revision` as runtime version

Rejected. `meta.revision` is an editing-time CAS value. It changes on every
admin write and exists to reject stale writers. Runtime versions are immutable
published artifacts and need stable historical reads, content hashes, archive
state, atomic publications, and monotonic rollback-by-copy semantics.

### Encoding versions into `AgentSpec.id`

Rejected. Runtime ids should remain logical resource ids. Encoding versions in
ids makes handoff, delegation, permissions, tracing, and UI references depend on
string parsing instead of an explicit server-owned pin.

### Materializing current runs from independent resource pointers

Rejected. Reading `agent.current_version`, `skill.current_version`, and provider
pointers independently can create a manifest that never existed as a published
configuration. Current-policy materialization must use a committed
`RegistryPublication`.
