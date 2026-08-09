#!/usr/bin/env python3
"""Enforce Awaken crate dependency, vocabulary, and core/extension boundaries."""
from __future__ import annotations
import re
import sys
from pathlib import Path
import _arch_fitness
import _coordinator_authority_fitness
import _crate_dependency_fitness
import _migration_fitness
from _executable_agent_boundary import EXECUTABLE_AGENT_ALLOWED_DEPS
import _provider_env_fitness
from _privacy_boundary import PRIVACY_ALLOWED_DEPS
import _resource_plane_fitness
import _runtime_secret_boundary
import _service_data_ownership_fitness
from _sandbox_policy_boundary import SANDBOX_POLICY_ALLOWED_DEPS
from _crate_boundary_workspace import (
    architecture_fitness_specs,
    check_bucket_direction,
    iter_crate_manifests,
    load_manifest,
    package_name,
    text_files,
)
from _managed_routers_boundary import MANAGED_ROUTERS_ALLOWED_DEPS
from _managed_protocol_boundary import MANAGED_PROTOCOL_ALLOWED_DEPS
REPO_ROOT = Path(__file__).resolve().parent.parent.parent
CRATES = REPO_ROOT / "crates"
# Async runtime infrastructure (not domain or provider types) is permitted in
# neutral crates: async-trait makes the ports dyn-safe, tokio drives execution,
# tokio-util carries the cancellation token. A model/provider SDK such as genai
# is deliberately NOT in this set for any neutral crate; only provider adapters use it.
ALLOWED_DEPS: dict[str, set[str]] = {
    **SANDBOX_POLICY_ALLOWED_DEPS,
    **MANAGED_ROUTERS_ALLOWED_DEPS,
    **MANAGED_PROTOCOL_ALLOWED_DEPS,
    **EXECUTABLE_AGENT_ALLOWED_DEPS,
    **PRIVACY_ALLOWED_DEPS,
    # zeroize backs RedactedString's zero-on-drop (ADR-0043); a leaf crypto-hygiene
    # primitive, not a model/provider SDK.
    "awaken-agent-contract": {"serde", "serde_json", "thiserror", "async-trait", "tokio", "zeroize", "http", "schemars"},
    # Cross-context, secret-free credential execution values and the exact
    # material-resolution port (ADR-0067). Vault/storage and runtime adapters
    # depend inward on this leaf; the leaf names neither implementation.
    "awaken-credential-contract": {
        "awaken-agent-contract",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
    },
    # The "judgment" half of access control: authenticate + authorize over the
    # shared in-memory iam engine (the durable iam-server store is wired by the
    # composition root, not named here).
    "awaken-authz-enforce": {
        "awaken-iam-contract",
        "awaken-iam-core",
        "awaken-iam-preset",
        # Tenancy edge aspect (ADR-0051 D4): opaque `ScopeId` + `resolve_scope`.
        "awaken-tenancy",
        "axum",
        "serde_json",
        # Stable, non-reversible internal thread ids for application scopes.
        "sha2",
        # dev-only: guard middleware tests drive a minimal axum router.
        "tokio",
        "tower",
        # dev-only: property-based (formal) verification (ADR-0059).
        "proptest",
    },
    # Webhooks (ADR-0048 / S10): signing, event shape, and HTTP delivery only. No dep on
    # the Managed wire crate (it takes event type / id / tenancy as data) and no
    # subscription store: subscriptions live in the config plane, reached through the
    # SubscriptionSource port with secrets already resolved.
    "awaken-webhook": {
        "serde",
        "serde_json",
        "hmac",
        "sha2",
        "base64",
        "subtle",
        "getrandom",
        "async-trait",
        "reqwest",
        "tokio",
        # dev-only: the e2e stands up a real axum receiver on an ephemeral port.
        "axum",
    },
    # The managed webhook bridge (ADR-0048 / S10): connects protocol-managed's
    # SessionLifecycleSink to the neutral dispatcher + subscription CRUD + the
    # guard→WorkspaceScope map. Open crates only, so the standalone shares it.
    "awaken-webhook-managed": {
        "awaken-session-contract",
        "awaken-resource-contract",
        "awaken-tenancy",
        "awaken-authz-enforce",
        "awaken-webhook",
        # dev-only: the e2e stands up a real axum receiver (tower util + body reading).
        "tower",
        "http-body-util",
        # Subscriptions are a config resource: the read-side WebhookStore port + the
        # secret-free WebhookEndpointDef, and the vault SecretStore the whsec_ key is
        # sealed behind (RedactedString at the seam). All open, port-only — the durable
        # admin backend is injected by the assembly, never named here.
        "awaken-config-resolver",
        "awaken-credential-vault",
        "awaken-agent-contract",
        "async-trait",
        "axum",
        "serde_json",
        "tokio",
    },
    # Tenancy as an edge aspect (ADR-0051): the opaque `ScopeId` + the pure
    # ingress reconciliation (`resolve_scope`). A foundation leaf, serde-only;
    # names no iam/store/wire — the `ScopeId → ScopeRef` ACL lives in the PDP
    # adapter, never here.
    # dev-only: serde_json drives the ScopeId wire (scope_id column) round-trip test.
    "awaken-tenancy": {"serde", "serde_json"},
    # The neutral session-runtime ports + signature vocabulary (session runtime, work
    # queue, MCP probe/target identity, agent-config source, session repo), extracted from the Managed
    # wire adapter so the host + other implementors depend on a contract/ leaf, not on
    # a protocol adapter. Dependencies point inward — agent-domain vocab + async-trait
    # only; names no wire, store, or plane. Ports move here incrementally.
    "awaken-session-contract": {
        "awaken-agent-contract",
        "awaken-credential-contract",
        # Session resolution consumes only the resources-plane identity/config
        # port. Authorization remains an edge/PDP concern.
        "awaken-resource-contract",
        "awaken-tenancy",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "http",
        "tokio",
        # dev-only: property-based verification of the WorkState wire bijection (ADR-0059).
        "proptest",
    },
    # Resources-plane ports (FileStore / MemoryRepository / SkillStore) +
    # the value/error types in their signatures — mirrors awaken-provisioning-contract.
    # A foundation leaf: no backend, SQL driver, or filesystem, so an adapter reusing
    # these stores depends on the traits alone. The backends re-export it.
    # dev-only: serde_json drives the Memory wire-contract round-trip tests.
    # proptest is dev-only (property-based verification of validate_path_len, ADR-0059).
    "awaken-resource-contract": {"async-trait", "thiserror", "serde", "serde_json", "proptest"},
    # Cross-cutting telemetry infrastructure (NOT an `ext-*`): the process-global
    # tracing subscriber + OTLP / AWAKEN_TRACE_FILE span export + W3C traceparent
    # propagator + the axum ingress span middleware. A foundation leaf consumed by
    # the service binaries; external deps only, names no domain capability.
    "awaken-observability": {
        "serde_json",
        "axum",
        "futures",
        "tracing",
        "tracing-subscriber",
        "tracing-opentelemetry",
        "opentelemetry",
        "opentelemetry_sdk",
        "opentelemetry-otlp",
        # The Prometheus scrape exporter (#4): one SdkMeterProvider feeds both an
        # OTLP push reader and a Prometheus pull reader over the shared registry.
        "opentelemetry-prometheus",
        "prometheus",
        # The neutral MetricsRecorder port (#2): the OTel-backed recorder impl
        # lives here beside the Meter it feeds. A foundation contract leaf — no
        # domain capability travels, only the structure-only metric vocabulary.
        "awaken-runtime-contract",
        # dev-only: trace_http span-middleware tests drive a real axum Router via
        # tower::ServiceExt::oneshot on a tokio runtime.
        "tokio",
        "tower",
        # dev-only: property-based (formal) verification (ADR-0059).
        "proptest",
    },
    # Host-side credential vocabulary (Credential / AuthChallenge /
    # CredentialRefresher) shared by the outbound wire clients (awaken-ext-mcp,
    # awaken-protocol-a2a). A leaf like the contracts: names no wire, store, or
    # runtime type, so clients depend on it without pulling anything upward.
    "awaken-credential": {"async-trait", "tokio"},
    # Management plane (ADR-0043), agents bucket — orthogonal to execution; the
    # runtime never depends on these (I4 / D6/D9, enforced by check_bucket_direction).
    "awaken-model-catalog": {
        "serde",
        "serde_json",
        "thiserror",
        "async-trait",
        "schemars",
        "awaken-scoped-migration",
        # ADR-0005: the sync rusqlite runner lives in the sibling crate.
        "awaken-scoped-migration-sqlite",
        "tokio",
        # feature `sqlite`: embedded durable CatalogRepo backend over the crate's
        # own `catalog` migration scope (ADR-0043 sqlite-repos).
        "rusqlite",
        "sqlx",
        # feature `postgres`: network-DB CatalogRepo backend over the same
        # `catalog` migration scope (ADR-0043); the SQL driver, as config-store.
        "sqlx",
        # dev-only: reopen-from-file persistence tests.
        "tempfile",
    },
    "awaken-credential-vault": {
        "awaken-agent-contract",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "schemars",
        # feature `sealed-aead`: AEAD encryption-at-rest for the secret store.
        "chacha20poly1305",
        "tokio",
        "awaken-scoped-migration",
        # ADR-0005: the sync rusqlite runner lives in the sibling crate.
        "awaken-scoped-migration-sqlite",
        # feature `sqlite`: embedded durable CredentialRepo + SealedBlobStore
        # backends over the crate's own `credential` migration scope (ADR-0043
        # sqlite-repos); the durable secret path stays sealed-only.
        "rusqlite",
        # feature `postgres`: network-DB CredentialRepo + SealedBlobStore backends
        # over the same `credential` migration scope (ADR-0043); the SQL driver.
        "sqlx",
        # dev-only: reopen-from-file persistence tests.
        "tempfile",
    },
    # The config-authoring plane (ADR-0036/slice A), extracted from awaken-runtime-host
    # so both the authoring plane (awaken-control) and the data plane (the host) can
    # share it without control depending on the execution host. Pure config-plane logic:
    # the config service + CRUD/capabilities routers, the model-binding resolver, and the
    # scoped tool catalog — it names neither SharedHost nor run execution.
    "awaken-config-service": {
        "awaken-agent-contract",
        "awaken-executable-agent-contract",
        "awaken-executable-agent-catalog",
        "awaken-runtime-contract",
        "awaken-session-contract",
        "awaken-config-store",
        "awaken-config-resolver",
        # Secret-free resource identity/config ports; IAM stays a sibling plane.
        "awaken-resource-contract",
        "awaken-tenancy",
        "awaken-ext-memory",
        "awaken-ext-builtin-tools",
        "awaken-ext-compact",
        "awaken-ext-permission",
        "awaken-ext-state-machine",
        "async-trait",
        "serde_json",
        "thiserror",
        "tokio",
        "axum",
    },
    "awaken-config-resolver": {
        "awaken-agent-contract",
        # Resource Catalog is a port-only inward dependency; it contains no IAM,
        # HTTP, storage adapter, or runtime-host type.
        "awaken-resource-contract",
        "awaken-runtime-contract",
        "awaken-model-catalog",
        "awaken-credential-vault",
        "serde",
        "thiserror",
        "tokio",
        "serde_json",
        # dev-only: E2E test drives the runtime provider adapter + managed ACL.
        "awaken-provider-genai",
        "awaken-managed-bridge",
        "schemars",
    },
    "awaken-admin-config-api": {
        "awaken-model-catalog",
        "awaken-credential-vault",
        "awaken-agent-contract",
        "awaken-api-contract",
        "awaken-config-resolver",
        # Schema-only dependency: export the config store's authoritative
        # ModelSelection wire instead of maintaining a parallel UI contract.
        "awaken-config-store",
        # Resource Catalog port only: resource identity, Workspace ownership,
        # config versions and lifecycle. Authorization remains at the PEP.
        "awaken-resource-contract",
        # The neutral `Disposition` resilience taxonomy: the ops cooldown routes map
        # a credential-probe failure onto a retry/cool-down policy (E3-4).
        "awaken-runtime-contract",
        # HTTP adapters consume the neutral trusted WorkspaceScope coordinate;
        # authorization remains at the composition PEP.
        "awaken-tenancy",
        "serde",
        "serde_json",
        "thiserror",
        "axum",
        "async-trait",
        "schemars",
        "tokio",
        "tower",
        "http-body-util",
        # The admin plane's own aggregates (profiles / MCP defs / agent bindings)
        # get a migration scope of their own (`awaken.admin`), like the
        # catalog/credential domains.
        "awaken-scoped-migration",
        # ADR-0005: the sync rusqlite runner lives in the sibling crate.
        "awaken-scoped-migration-sqlite",
        # feature `sqlite`: embedded durable InferenceProfileStore + McpStore
        # backend over the crate's own `admin` migration scope (ADR-0043
        # sqlite-repos).
        "rusqlite",
        # feature `postgres`: network-DB admin store over the same `admin`
        # migration scope (ADR-0043); the SQL driver, bridged to the sync ports
        # via a store-owned runtime.
        "sqlx",
        # dev-only: reopen-from-file persistence tests.
        "tempfile",
    },
    "awaken-managed-bridge": {
        "awaken-agent-contract",
        "awaken-credential-vault",
        "awaken-model-catalog",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
    },
    "awaken-runtime-contract": {
        "awaken-agent-contract",
        "awaken-credential-contract",
        # ADR-0062's published provider candidate carries one opaque owner pin
        # for credential integrity checks. This is data only: runtime receives
        # no WorkspaceScope, scope graph, repository selection, or IAM ability.
        "awaken-tenancy",
        "serde",
        "serde_json",
        "sha2",
        "thiserror",
        "async-trait",
        # `FutureExt::catch_unwind` isolates a panicking terminal observer while
        # preserving the caller task's neutral execution-local context.
        "futures-util",
        "tokio",
        "tokio-util",
        # Optional schema derive for contract generation; no runtime behavior.
        "schemars",
    },
    "awaken-runtime": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        # The in-memory reference store lives in its own backend crate (ADR-0039
        # 2.2); the kernel re-exports it at `memory` for its default local wiring.
        "awaken-store-inmem",
        "serde",
        "serde_json",
        "thiserror",
        "async-trait",
        "tokio",
        "tokio-util",
        # `catch_unwind` isolates a panicking third-party tool at the executor boundary.
        "futures-util",
        # Poison-free mutexes for the in-memory registries + circuit breaker.
        "parking_lot",
        "tracing",
        "tempfile",
    },
    # In-memory reference store backend: neutral commit/read ports (ADR-0039 2.2).
    "awaken-store-inmem": {
        "awaken-agent-contract",
        "awaken-store-conformance",
        "async-trait",
        "serde_json",
        "tokio",
    },
    # Filesystem backend: durable crash-safe append log (ADR-0039 2.3).
    "awaken-store-fs": {
        "awaken-agent-contract",
        "awaken-store-inmem",
        "awaken-store-conformance",
        "async-trait",
        "serde",
        "serde_json",
        "tokio",
    },
    # Trait-generic store conformance suite (ADR-0039 2.6): backend-agnostic
    # behavioural checks every store backend runs from its own test crate.
    "awaken-store-conformance": {
        "awaken-agent-contract",
        "serde_json",
        # `join!` drives the concurrent-append case portably (no per-backend spawn).
        "tokio",
    },
    # Dispatch / run-ingress contract (ADR-0039 2.1): the durable-dispatch port
    # surface, factored out of the host so backends can depend on it (G2).
    "awaken-run-ingress-contract": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-tenancy",
        "awaken-worker-contract",
        # Serializable claim/settle payloads (the durable-persistence serde contract).
        "serde_json",
        "async-trait",
        "serde",
        "thiserror",
        # Dev-only: drive the async port default methods in unit tests.
        "tokio",
    },
    # Public, backend-agnostic conformance driver for DispatchQueue adapters and
    # decorators. It depends only on the durable wire contract and the value types
    # needed to construct a self-contained RunDispatch fixture; production crates
    # consume it only as a dev-dependency.
    "awaken-run-ingress-testkit": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-run-ingress-contract",
        "serde_json",
    },
    # Provisioning contract: the neutral, data-only sandbox vocabulary and ports
    # (SandboxProvider / Sandbox / prepare_environment / admission). Names no OS
    # mechanism, host path, wire, or runtime type, so concrete realizers (lexical /
    # namespace / container) depend on it without pulling anything upward (G2/G3).
    "awaken-provisioning-contract": {
        "awaken-agent-contract",
        # Session owns the frozen eager/on-tool-use timing value; providers
        # consume that contract rather than defining a parallel policy enum.
        "awaken-session-contract",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",  # dev-only: fake async ports
    },
    # Worker fleet vocabulary and pure placement kernel. It composes the existing
    # provisioning capability vocabulary; persistence and channels stay in server
    # adapters, so the isolated worker contract remains store-free.
    "awaken-worker-contract": {
        "awaken-acp-contract",
        # Credential identity and observation state have one canonical owner;
        # Worker adds lease-validity evidence instead of redefining either value.
        "awaken-credential-contract",
        "awaken-provisioning-contract",
        "async-trait",
        "serde",
        "serde_json",
        "sha2",
        "thiserror",
        # dev-only property verification.
        "proptest",
    },
    # Content-addressed blob store (ADR-0041): the neutral file-store trait +
    # `content_id` (BLAKE3) + local backends (fs/in-mem). Network backends
    # (postgres/s3) are separate crates over the same trait. A leaf — names no
    # provider, runtime, or host-path type.
    "awaken-file-store": {
        # The port-only contract this crate implements and re-exports (FileStore).
        "awaken-resource-contract",
        "async-trait",
        "thiserror",
        "blake3",
        "tokio",
        # feature `postgres`: bytea backend over sqlx, versioned via the foundation's
        # scoped migration (the `awaken.file_store` bundle; no raw CREATE TABLE).
        "sqlx",
        "awaken-scoped-migration",
        # feature `sqlite`: embedded BLOB backend over the same `file_store` scope
        # (ADR-0005: the sync rusqlite runner lives in the sibling crate).
        "rusqlite",
        "awaken-scoped-migration-sqlite",
        # feature `s3`: object-store backend (S3/MinIO/GCS/Azure) + its list stream.
        "object_store",
        "futures",
        # dev-only: temp dirs for the fs backend round-trip test.
        "tempfile",
    },
    # Durable path-addressed memory persistence (resources plane), backed by a
    # scoped-migration bundle over SQLite with an optional Postgres sibling. It names
    # no runtime, authorization, workspace, principal, role, or provider type.
    "awaken-memory-store": {
        # The port-only contract this crate implements and re-exports
        # (MemoryRepository + Memory/MemoryEntry/MemErr).
        "awaken-resource-contract",
        "async-trait",
        "thiserror",
        "tokio",
        "rusqlite",
        # feature `postgres`: the multi-node MemoryRepository backend.
        "sqlx",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        # ADR-0053 path-addressed MemoryRepository: content hashing + legacy-version import.
        "sha2",
        "serde_json",
        # dev-only: SQLite conformance tests open temporary database files.
        "tempfile",
    },
    # ADR-0053: the write-through memory-store FUSE server. A `server`-bucket crate
    # (it depends on the resources-tier `MemoryRepository` and links `fuser`), realizing the
    # provisioning-contract `MountSource::MemoryStore` → `Realization::Fuse`.
    "awaken-sandbox-memoryd": {
        "awaken-memory-store",
        "awaken-provisioning-contract",
        "async-trait",
        "thiserror",
        "tokio",
        "tracing",
        "fuser",
        "libc",
        # dev-only: the kernel-VFS test issues a `truncate` by path (setattr without
        # an fd) via nix's safe wrapper.
        "nix",
    },
    # ADR-0056 §4: the worker-plane orchestrator for isolation-instance reuse. Owns the
    # reconcile/renew control flow and delegates every judgement to the pure decision
    # kernel in awaken-provisioning-contract (reconcile_adoption/decide_reap) — so it
    # depends on the contract and nothing heavier (no host, no store, no runtime).
    "awaken-sandbox-manager": {
        "awaken-provisioning-contract",
        # dev-only: drive the async ports through a recording fake in unit tests.
        "async-trait",
        "tokio",
    },
    # Durable Skill aggregate repository (resources plane): complete immutable,
    # binary-safe bundles over filesystem/sqlite/postgres. It names no runtime,
    # authorization, principal, role, or policy type.
    "awaken-skill-store": {
        # The port-only contract this crate implements and re-exports (SkillStore).
        "awaken-resource-contract",
        "async-trait",
        "thiserror",
        "tokio",
        # Aggregate serialization and canonical bundle integrity hashing.
        "serde",
        "serde_json",
        "sha2",
        "zip", "rusqlite",
        # feature `postgres`: the multi-node SkillStore backend.
        "sqlx",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        # dev-only: conformance + reopen-from-file persistence tests.
        "tempfile",
    },
    # Agent-transport seam (ADR-0041 amendment): the segregated `AgentChannel`
    # duplex + `AgentTransport` capability port, kept off `ProcessHandle` (ISP).
    # A leaf over tokio's async IO traits; names no provider, protocol, or host
    # path, so a tool-transparent provider and the ACP bridge both depend on it.
    "awaken-agent-channel": {
        "async-trait",
        "thiserror",
        "tokio",
    },
    # Neutral ACP capability descriptors and the channel-in/descriptor-out
    # handshake port. Worker applications depend on this leaf; protocol
    # adapters implement it.
    "awaken-acp-contract": {
        "awaken-agent-channel",
        "async-trait",
        "serde",
        "sha2",
    },
    # Connection plan (ADR-0045): the topology value object (ConnectionPlan /
    # DialAddr / Wiring / DialPolicy / CredentialRef) + ChannelFactory over the
    # agent-channel duplex. A provisioning leaf — names no runtime, model, or
    # store type; carries a CredentialRef, never resolved material (G34).
    "awaken-connection-plan": {
        "awaken-agent-channel",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
    },
    # Remote hand (ADR-0044): RemoteToolExecutor + serve_hand + the HandRequest/
    # HandReply wire over the neutral ToolCall/ToolOutput value objects. It depends
    # on the runtime CONTRACT only — no kernel, model, commit, or store (G33). The
    # composition end-to-end test lives in awaken-runtime-examples, not here.
    "awaken-tool-relay": {
        "awaken-runtime-contract",
        # The channel-bound marker (ADR-0045 D6): the hand's framed I/O runs over
        # any `AgentChannel`, the one bound shared by every brain/hand use site.
        # A pure marker (no kernel/store/model), so G33 isolation is unaffected.
        "awaken-agent-channel",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        "tokio-util",
        "futures-util",
        "bytes",
    },
    # Builtin tools extension: owns concrete tool ids and runs them in-process
    # (ADR-0007). It implements the async `Tool`/`RawTool` ports (`async-trait`,
    # `tokio`), hand tools touch the filesystem (`glob`, `regex`, `tempfile` for
    # tests), and the network tools make HTTP calls (`ureq`). These execution
    # deps live here, never in a neutral crate.
    "awaken-ext-builtin-tools": {
        "awaken-runtime-contract",
        "async-trait",
        "glob",
        "regex",
        "serde",
        "serde_json",
        "tempfile",
        "tokio",
        "ureq",
    },
    # Permission policy extension: declarative allow/ask/deny rules over the
    # neutral PermissionPolicy port. Matching is delegated to the shared
    # tool-pattern engine. No runtime-core or store deps.
    "awaken-ext-permission": {
        "awaken-runtime-contract",
        "awaken-tool-pattern",
        "async-trait",
        "serde",
        "serde_json",
        "tokio",
    },
    # Shared tool-call pattern engine: parses and matches the pattern DSL
    # (glob/regex/exact tool names, primary-arg globs, nested-field conditions).
    # A neutral leaf utility over serde JSON with glob/regex matchers; it names
    # no domain, provider, runtime, or store type, so both the permission policy
    # and the state-machine FSM can share one DSL.
    "awaken-tool-pattern": {
        "serde",
        "serde_json",
        "glob-match",
        "regex",
        "thiserror",
    },
    # Provider adapter: the only crate allowed to name the model SDK. It also
    # consumes the SDK's async response stream and implements provider HTTP
    # model-directory discovery, so `futures` and `reqwest` stay at this wire
    # adapter boundary rather than leaking into catalog/application code.
    "awaken-provider-genai": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "genai",
        "async-trait",
        "futures",
        "reqwest",
        "tokio",
        "serde",
        "serde_json",
        "thiserror",
    },
    # Durable commit schema: the portable migration bundle shared by every store
    # backend. It names the migrator but no SQL driver, so the same schema drives
    # both the Postgres and SQLite runners without duplication (ADR-0012).
    "awaken-store-schema": {
        "awaken-scoped-migration",
        # dev-deps: prove the portable bundle APPLIES on both real backends it
        # drives (embedded SQLite + skip-on-unreachable Postgres). The lib names no
        # SQL driver; these runners/drivers live only in the test build.
        "awaken-scoped-migration-sqlite",
        "rusqlite",
        "sqlx",
        "tokio",
    },
    # Postgres durable store: a crate allowed to name the SQL driver (`sqlx`) and
    # the migration runner. It implements the neutral CommitCoordinator / read
    # ports against Postgres; the driver lives here, not in runtime core
    # (ADR-0006 commit contract, ADR-0007 boundary).
    "awaken-store-postgres": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-store-schema",
        "awaken-scoped-migration",
        "async-trait",
        "sqlx",
        "tokio",
        "serde",
        "serde_json",
        "thiserror",
        # dev-dep: run the shared store conformance suite against Postgres so it
        # cannot diverge from the other backends (ADR-0039 2.6).
        "awaken-store-conformance",
    },
    # Config domain store: compiles a declarative config into a published
    # snapshot/install and persists it under the `config` namespace. Depends on
    # the contract (the published language) and the store drivers; the dev-only
    # round-trip test composes the runtime.
    "awaken-config-store": {
        "awaken-runtime-contract",
        "awaken-runtime",
        "awaken-agent-contract",
        # Tenancy edge aspect (ADR-0051 D4): the opaque `ScopeId` the
        # `ScopedConfig` decorator binds so the authoring aggregate is
        # tenant-isolable (one `scope_id` column, `WHERE scope_id = ?`).
        "awaken-tenancy",
        "awaken-store-sqlite",
        "awaken-scoped-migration",
        # ADR-0005: the sync rusqlite runner lives in the sibling crate.
        "awaken-scoped-migration-sqlite",
        "async-trait",
        "sqlx",
        "rusqlite",
        "sha2",
        "tokio",
        "serde",
        "serde_json",
        "thiserror",
        # Optional JsonSchema implementation delegates to the existing serde
        # wire enum, keeping serialization and generated UI types co-owned.
        "schemars",
    },
    # Host-layer teaching examples: the one place that wires every concrete
    # adapter into a runnable runtime. Composition root, so it may name them all.
    "awaken-runtime-examples": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-runtime",
        "awaken-ext-permission",
        # dev-dep: `hello_agent` borrows the config domain's pure `compile()`.
        "awaken-config-store",
        # dev-deps: `memory_skills_combo` is the bare-Runtime assembly recipe for
        # the two extensions together — it proves their PUBLIC exports suffice to
        # wire memory + skills without the host's private wiring.
        "awaken-ext-memory",
        "awaken-ext-skills",
        # dev-deps: `remote_hand_e2e` composes the brain (Runtime) with a remote
        # hand (awaken-tool-relay) over a topology plan (awaken-connection-plan) —
        # the ADR-0044/0045 composition root lives here, not in the leaf crates.
        "awaken-tool-relay",
        "awaken-connection-plan",
        # dev-dep: `tcp_relay_e2e` drives the brain↔hand relay over a real TCP
        # socket, so it needs the agent-channel transport the topology plan wires.
        "awaken-agent-channel",
        # dev-deps: `remote_hand_sandbox_e2e` runs the remote hand's tool inside a
        # real OS sandbox (ADR-0044 hand × ADR-0041 sandbox) — the composition root
        # for "the hand executes tools under isolation" lives here.
        "awaken-sandbox-local",
        "awaken-provisioning-contract",
        # coding-agent example (feature-gated): built-in tools, a real model, a TUI.
        "awaken-ext-builtin-tools",
        "awaken-provider-genai",
        "genai",
        "ratatui",
        "crossterm",
        "anyhow",
        "async-trait",
        "serde_json",
        "tokio",
    },
    # Durable WorkQueue backend (the self-hosted environment work queue): sqlite +
    # postgres at parity, implementing the session contract's `WorkQueue` port.
    # Extracted from awaken-runtime-host so the host stays lean (Step 3b).
    "awaken-work-store": {
        "awaken-session-contract",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        "async-trait",
        "rusqlite",
        "sqlx",
        "serde",
        "serde_json",
        "tokio",
        # dev-only: property-based (formal) verification of the store's invariants.
        "proptest",
    },
    # Durable EnvRegistry backend (the self-hosted environment registry): sqlite +
    # postgres at parity, implementing the session contract's env-registry port.
    # Extracted from awaken-runtime-host so the host stays lean (Step 3b).
    "awaken-env-store": {
        "awaken-session-contract",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        "async-trait",
        "rusqlite",
        "sqlx",
        "serde_json",
        "tokio",
        "tempfile",
        # dev-only: property-based (formal) verification of the store's invariants.
        "proptest",
    },
    # Durable ManagedSessionRepository backend (the managed session aggregate): sqlite +
    # postgres at parity, implementing the session contract's repository port. Extracted
    # from awaken-runtime-host so the host stays lean (Step 3b).
    "awaken-session-store": {
        "awaken-agent-contract",
        "awaken-session-contract",
        "awaken-deployment-contract",
        "awaken-credential-contract",
        # ADR-0064: Memory extraction aggregate/repository port belongs to the
        # Memory Runtime Extension; this store is one persistence adapter.
        "awaken-ext-memory",
        # The opaque tenancy ScopeId the in-memory scoped-session store keys its
        # isolation fence by (durable backends bind it as an opaque column).
        "awaken-tenancy",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        "async-trait",
        "rusqlite",
        "sqlx",
        "serde",
        "serde_json",
        "tokio",
        # dev-only: the sqlite backend tests open a temp on-disk database.
        "tempfile",
        # dev-only: property-based (formal) verification of the store's invariants.
        "proptest",
    },
    # SQLite durable store: the sibling backend, allowed to name the `rusqlite`
    # driver. Same neutral commit/read ports, embedded engine (ADR-0012).
    "awaken-store-sqlite": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-store-schema",
        "awaken-store-conformance",
        "awaken-scoped-migration",
        # ADR-0005: the sync rusqlite runner lives in the sibling crate.
        "awaken-scoped-migration-sqlite",
        "async-trait",
        "rusqlite",
        "tokio",
        "serde",
        "serde_json",
        "thiserror",
    },
    # Durable run host: the dispatch/server layer above the runtime kernel
    # (ADR-0009). It depends on the kernel it drives and the store adapter it
    # persists through, and — like an adapter — may name the SQL driver for its
    # own durable dispatch tables. Nothing below the kernel depends on it.
    "awaken-run-ingress": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-run-ingress-contract",
        # dev-only: run the public DispatchQueue conformance suite against every
        # in-tree backend.
        "awaken-run-ingress-testkit",
        "awaken-runtime",
        "awaken-ext-builtin-tools",
        "awaken-observability",
        "tracing",
        # The worker HTTP dispatch client (`HttpDispatchQueue`) posts claim/settle.
        "reqwest",
        # dev-only: the transport_client e2e stands up a real axum server mirroring a
        # cell server's dispatch_transport_router so the client crosses a real socket.
        "axum",
        "awaken-scoped-migration",
        # ADR-0005: the sync rusqlite runner lives in the sibling crate.
        "awaken-scoped-migration-sqlite",
        "awaken-store-postgres",
        "awaken-store-sqlite",
        "async-trait",
        "sqlx",
        "rusqlite",
        "async-nats",
        "futures-lite",
        "tokio",
        "tokio-util",
        "serde",
        "serde_json",
        "sha2",
        "thiserror",
    },
    # Durable worker-directory adapter. One shared transition kernel drives its
    # memory/SQLite/Postgres stores; the neutral identity/placement vocabulary
    # remains in the store-free worker contract.
    "awaken-worker-registry": {
        "awaken-worker-contract",
        "async-trait",
        "serde_json",
        "thiserror",
        "tokio",
        "rusqlite",
        "sqlx",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        # Dev-only verification tools and fixtures. Loom is never a production
        # dependency; the registry swaps its mutex only under cfg(test).
        "loom",
        "proptest",
        "tempfile",
    },
    # Shared protocol transport seam: the neutral vocabulary every streaming /
    # request-response adapter drives (`ProtocolRuntime` + its step/pending/resume
    # value objects) plus the wire-agnostic `blocks_text` helper, below the wire-DTO
    # layer. It names only the agent-domain contract (for `Message`/`ContentBlock`)
    # and no wire type, so AG-UI, AI SDK, and A2A share one copy instead of three.
    "awaken-protocol-transport": {
        "awaken-agent-contract",
        "async-trait",
        "serde_json",
        "thiserror",
        # `ChannelStreamSink` forwards live stream events onto an mpsc channel a
        # streaming adapter drains (the tool-input streaming path); `sync` only.
        "tokio",
    },
    # ACP bridge + supervisor (ADR-0041 Slice 3): the anti-corruption boundary
    # between an opaque agent's protocol stream and the neutral runtime. It drives
    # an `AgentChannel` (the transport seam) + `ProcessHandle` and projects events
    # through the `RunEventSink` binding port; it constructs no store. Agents plane.
    "awaken-protocol-acp": {
        "awaken-acp-contract",
        "awaken-agent-channel",
        "awaken-provisioning-contract",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        # feature `real-acp`: the official ACP codec, projected through the same ACL.
        "agent-client-protocol",
    },
    # ACP run executor: an external ACP agent as a peer RunExecutor. Runtime plane;
    # drives a channel via the Supervisor, classifies failures, commits through the
    # coordinator. Host opens the channel (injected), so no config/sandbox dep.
    "awaken-run-executor-acp": {
        "awaken-runtime-contract",
        "awaken-agent-contract",
        "awaken-provisioning-contract",
        "awaken-local-process",
        "awaken-agent-channel",
        "awaken-protocol-acp",
        "async-trait",
        # McpServerConfig derives Serde to ride the config plane into plugin_config.
        "serde",
        "thiserror",
        "tokio",
        # dev-only: build a `plugin_config` JSON value in the compaction-window test.
        "serde_json",
        # dev-only: temp dirs for the session-home reference impl's recovery tests.
        "tempfile",
    },
    # Worker-side ACP application service: discovers trusted-host adapters,
    # acquires catalog-pinned wrappers, registers secret-free WorkerLocal
    # bindings, and exposes liveness. It composes executor catalog metadata and
    # credential ports but owns neither execution nor durable storage.
    "awaken-acp-application": {
        "awaken-run-executor-acp",
        # The Worker application starts the bounded probe, but depends only on
        # the neutral handshake port; a composition root injects a protocol
        # adapter implementation.
        "awaken-acp-contract",
        "awaken-agent-channel",
        "awaken-runtime-contract",
        "awaken-credential-vault",
        "async-trait",
        # Capability fingerprints fence mutable, non-secret Worker evidence.
        "sha2",
        "tokio",
    },
    # A2A executor: a remote A2A agent (Coze / A2A HTTP) as a peer RunExecutor.
    # Runtime plane; foundation contracts + the A2A protocol crate only, like the
    # ACP executor — it constructs no config and names no secret.
    "awaken-run-executor-a2a": {
        "awaken-runtime-contract",
        "awaken-agent-contract",
        "awaken-protocol-a2a",
        "async-trait",
        "tokio",
        # The RemoteAgent adapter serializes the A2A discovery card to neutral JSON.
        "serde_json",
        # dev-only: a real localhost HTTP server the executor dials over real TCP.
        "axum",
    },
    # AI SDK v6 protocol adapter: the anti-corruption boundary between the Vercel
    # AI SDK UI Message Stream wire and the neutral runtime. Like the managed
    # adapter it owns public DTOs + the axum router, depends only on the
    # agent-domain contract (for `Message`/`project`), and drives an `AiSdkRuntime`
    # port, so it constructs no runtime.
    "awaken-protocol-ai-sdk": {
        "awaken-agent-contract",
        "awaken-api-contract",
        "awaken-tenancy",
        "awaken-protocol-transport",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        "axum",
        "tokio-stream",
        "tower",
        "http-body-util",
    },
    # AG-UI protocol adapter: the anti-corruption boundary between the AG-UI wire
    # and the neutral runtime. Same shape as the other protocol adapters; drives an
    # `AgUiRuntime` port and constructs no runtime.
    "awaken-protocol-ag-ui": {
        "awaken-agent-contract",
        "awaken-api-contract",
        "awaken-tenancy",
        "awaken-protocol-transport",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        "axum",
        "tokio-stream",
        "tower",
        "http-body-util",
    },
    # MCP protocol adapter (server side): exposes an explicit export set of
    # runtime tools (`RawTool` + `ToolDescriptor`) to external MCP clients over
    # stdio and Streamable HTTP — the egress mirror of the awaken-ext-mcp client
    # (ingress). Speaks the wire through awaken-mcp-wire and the `mcp` SDK's
    # types; guards calls with the contract's `ToolGateHook`. awaken-ext-mcp is
    # dev-only: the e2e tests drive this server with our own client.
    "awaken-protocol-mcp": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-mcp-wire",
        "awaken-mcp-server-core",
        # Dev-only: the same black-box suite independent host adapters run.
        "awaken-mcp-server-testkit",
        "awaken-ext-mcp",
        "async-trait",
        "serde",
        "serde_json",
        "tokio",
        "tokio-stream",
        "axum",
        "uuid",
        "mcp",
        "tower",
        "http-body-util",
    },
    # A2A protocol adapter: the anti-corruption boundary between the A2A HTTP+JSON
    # wire and the neutral runtime. It supports request/response plus SSE streams
    # while driving a `ProtocolRuntime` port and constructing no runtime.
    "awaken-protocol-a2a": {
        "awaken-agent-contract",
        "awaken-credential",
        "awaken-protocol-transport",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        "tokio-stream",
        "axum",
        "tower",
        "http-body-util",
        "ureq",
    },
    # Goal / outcome extension: goal vocabulary, a deterministic grader, and a
    # run-end continuation guard that drives the grade→revise loop inside the
    # runtime. Depends only on the runtime contract (like `awaken-ext-permission`).
    "awaken-ext-goal": {
        "serde",
        "serde_json",
        "async-trait",
        "awaken-runtime-contract",
        "tokio",
    },
    # Management ("admin") assistant (ADR-0052): the four read-only management tools
    # and their descriptors plus the seeded prompt. A leaf that names only the neutral
    # tool contract and the config aggregate (for drafting/validating AgentConfigs);
    # the host implements its CapabilityReader/DraftValidator ports.
    "awaken-admin-assistant": {
        "serde",
        "serde_json",
        "async-trait",
        "tracing",
        "awaken-runtime-contract",
        "awaken-config-store",
        "tokio",
    },
    # Memory extension: cross-session memory as a bounded context — the file store,
    # the `write_memory` tool, the extractor agent's config/prompts, and bounded
    # recall. Depends only on the runtime contract (like `awaken-ext-goal`); the
    # host wires it onto the aux-agent substrate.
    "awaken-ext-memory": {
        "serde",
        "serde_json",
        "async-trait",
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "tokio",
    },
    # Compaction extension: within-session context compaction — the compactor
    # agent's config/prompts and the pure fold policy. Depends only on the runtime
    # contract; the host wires it onto the aux-agent substrate.
    "awaken-ext-compact": {
        "serde",
        "serde_json",
        "async-trait",
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "tokio",
    },
    # Shared MCP wire layer: the direction-neutral JSON-RPC peer, SSE parser,
    # and progress vocabulary spoken by both the MCP client (awaken-ext-mcp) and
    # the MCP server (awaken-protocol-mcp). A leaf like awaken-credential: it
    # owns the small serde DTO set used on the wire, never a runtime, store, or
    # third-party MCP client SDK type.
    "awaken-mcp-wire": {
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        "tokio-util",
    },
    # Runtime-neutral MCP server mechanics: lifecycle/version negotiation,
    # list/call dispatch, SDK mapping, notifications/progress/cancellation, and
    # the axum-free Streamable HTTP decision kernel. It must never name awaken's
    # agent/runtime/store/gate contracts; those belong to host adapters above it.
    "awaken-mcp-server-core": {
        "awaken-mcp-wire",
        "async-trait",
        "serde",
        "serde_json",
        "tokio",
        "http",
        "mcp",
        # Dev-only property checks of response-envelope invariants.
        "proptest",
    },
    # Black-box protocol conformance driver shared by independent MCP host
    # adapters. Production crates consume it only as a dev dependency.
    "awaken-mcp-server-testkit": {
        "awaken-mcp-server-core",
        "awaken-mcp-wire",
        "async-trait",
        # Shared HTTP conformance replies use the framework-neutral status and
        # header types directly; no axum/hyper runtime enters the testkit.
        "http",
        "serde_json",
        "tokio",
    },
    # MCP client extension: connects to external Model Context Protocol servers
    # and exposes their tools as runtime `RawTool`s. Like the other extensions it
    # depends only on the runtime contract; as an adapter to an external wire
    # protocol it may name the `mcp` SDK and its transport stack (`reqwest` for
    # HTTP, `nix` for stdio subprocess signals, `futures`/`tracing`). The kernel
    # stays out — sampling, credentials, and refresh reach it through host-injected
    # ports, not an `awaken-runtime` dependency. `awaken-runtime`/`awaken-agent-contract`
    # are dev-only, for the end-to-end tool-call test (composition root in tests).
    "awaken-ext-mcp": {
        "awaken-credential",
        "awaken-mcp-wire",
        "awaken-runtime-contract",
        "awaken-agent-contract",
        "awaken-runtime",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        "tracing",
        "futures",
        "mcp",
        "reqwest",
        "nix",
    },
    # Skills extension: fronts the whole skill set with a single `Skill` tool
    # (catalog in the descriptor, instructions in the tool result) — ADR-0036.
    # Like the other extensions it depends only on the runtime contract; the
    # kernel never learns the concept "skill".
    "awaken-ext-skills": {
        "awaken-runtime-contract",
        "awaken-agent-contract",
        "async-trait",
        "serde",
        "serde_json",
        "glob",
        "tokio",
    },
    # State-machine extension: a loadable FSM DSL that constrains tool-call order.
    # It contributes a gate, tool-outcome hook, and run-end guard through the
    # runtime contract, reads/writes state via the agent contract, and matches
    # tool calls with the shared `awaken-tool-pattern` crate. The kernel never
    # learns the concept "state machine".
    "awaken-ext-state-machine": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-tool-pattern",
        "async-trait",
        "serde",
        "serde_json",
        "serde_yaml",
        "thiserror",
        "url",
        "tokio",
        # optional (feature "schema"): derive a JSON Schema for the config type.
        "schemars",
        # dev-only: the end-to-end agent-loop test composes a runtime.
        "awaken-runtime",
    },
    # Sole local descendant-aware ProcessHandle; contains no sandbox/ACP policy.
    "awaken-local-process": {
        "awaken-provisioning-contract", "async-trait", "tokio", "nix", "tempfile",
    },
    # Worker tier (ADR-0041/ADR-0053): the isolated-execution sandbox provider. It
    # links NO durable store (A-G17) — it resolves mount bytes through the injected
    # `BlobSource` port and computes the BLAKE3 content id itself.
    "awaken-sandbox-local": {
        "awaken-runtime-contract",
        # Implements the neutral sandbox ports (ADR-0041): a LocalProvider over the
        # provisioning contract, alongside the pre-contract Environment surface.
        "awaken-provisioning-contract",
        "awaken-local-process",
        # BLAKE3 content id for the mount pin (was awaken_file_store::content_id).
        "blake3",
        # The tool-transparent capability: spawn_agent returns a pipe-backed AgentChannel.
        "awaken-agent-channel",
        "awaken-ext-builtin-tools",
        "async-trait",
        "serde_json",
        "thiserror",
        "tokio",
        # dev-only: temp dirs + a real content store behind a BlobSource test adapter,
        # plus the worker-tier memory mounter (FUSE/copy) wired via the MemoryMounter
        # port to prove memory-store realization end-to-end (ADR-0053 item 1).
        "tempfile",
        "awaken-file-store",
        "awaken-sandbox-memoryd",
        "awaken-memory-store",
    },
    # Container/K8s provider (ADR-0041 Slice 5): realizes the neutral sandbox ports
    # over a dependency-inverted ContainerRuntime port + pure plan renderers. The
    # real bollard/kube clients are adapters behind that port (added under features
    # in a distributed build); the neutral crate names none of them.
    "awaken-sandbox-container": {
        "awaken-provisioning-contract",
        # Dev-only Session proof drives ToolExecutor through the real hand wire.
        "awaken-runtime-contract",
        "awaken-tool-relay",
        # The container tier is tool-transparent: it hands the ACP bridge an
        # AgentChannel (network duplex) to the process-as-container agent.
        "awaken-agent-channel",
        "async-trait",
        "serde_json",
        "thiserror",
        # BLAKE3 content id: resolve File/Resource bytes through the injected BlobSource
        # port and verify the declared hash itself (A-G17, parity with sandbox-local).
        "blake3",
        # feature `connection`: awaken-connection establishes the remote AgentChannel
        # (TCP dial + reverse dial); tokio provides the net stack. Both optional.
        "awaken-connection",
        "tokio",
        # Bounded tar decoding for backend-neutral file harvesting over exec stdio.
        "tar",
        # feature `docker`: real Docker backend over the Engine API (SDK, not CLI).
        "bollard",
        "futures-util",
        # feature `k8s`: real Kubernetes backend over the apiserver (SDK, not kubectl).
        "kube",
        "k8s-openapi",
        # kube's rustls client needs a CryptoProvider (ring) installed explicitly.
        "rustls",
    },
    # Fixture-driven eval harness (#4): replays recorded cases through the real
    # runtime (RunExecutor) and scores them. Depends on the kernel to run cases.
    "awaken-eval": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-runtime", "awaken-ext-goal", "awaken-ext-compact", "awaken-ext-memory", "awaken-run-executor-acp",
        # Full-server Admin Assistant evaluation drives the public HTTP boundary;
        # request/response lifecycle stays in the eval adapter, not the runtime.
        "async-trait", "reqwest", "rusqlite", "thiserror",
        "serde",
        "serde_json",
        "tokio",
    },
    "awaken-runtime-host": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        # Session facts and APIs are consumed from their canonical owner; the
        # host must not obtain them through the Managed protocol facade.
        "awaken-session-contract",
        # Resource facts and lifecycle SPIs are consumed from their canonical
        # owner; the host must not obtain them through the Managed facade.
        "awaken-resource-contract",
        # dev-only: host tests use the canonical Resource catalog adapter.
        "awaken-resource-store",
        "awaken-runtime",
        "tower",
        # dev-only: the files/models routers were extracted to this sibling adapter;
        # runtime-host's files + resource-composition HTTP tests drive them.
        "awaken-managed-routers",
        # The OTel-backed metrics recorder injected into each per-thread runtime
        # (#2), so a server with OTLP configured exports model/tool metrics.
        "awaken-observability",
        "awaken-ext-builtin-tools",
        "awaken-ext-memory",
        "awaken-ext-compact",
        "awaken-ext-permission",
        "awaken-ext-skills",
        "awaken-ext-mcp",
        "awaken-ext-goal",
        "awaken-ext-state-machine",
        "awaken-sandbox-local",
        # The container tier (podman/docker/k8s) behind ContainerChannelSource, so an
        # agent can run inside a user image; the runtime backend is worker-configured.
        "awaken-sandbox-container",
        # The content-addressed store the host serves files/artifacts from and adapts
        # behind the sandbox providers' BlobSource port (was re-exported via
        # awaken-sandbox-local before that crate moved to the worker tier).
        "awaken-file-store",
        # The neutral sandbox vocabulary (SandboxSpec/Command/NetworkPolicy) the
        # sandboxed ACP channel source speaks when realizing the namespace tier —
        # already in the closure via awaken-sandbox-local; named directly here.
        "awaken-provisioning-contract",
        # dev-dep: the sandboxed-channel-source integration test observes what the
        # bwrap-confined agent said through the in-memory commit boundary.
        "awaken-store-inmem",
        "awaken-memory-store",
        "awaken-skill-store",
        "awaken-store-sqlite",
        "awaken-store-fs",
        # Multi-node durable commit coordinator (ADR-0022 D6). OPEN, like the other
        # store backends: a Postgres queue is table stakes for self-hosted durability,
        # not a paid differentiator — the closed line is placement/sharding/tenancy
        # (the horizontal-scaling fan-out ABOVE this store port), not the DB driver.
        "awaken-store-postgres",
        "awaken-work-store",
        "awaken-session-store",
        "awaken-env-store",
        # The config-authoring plane, extracted to a shared crate; the host re-exports
        # it (config service + routers + resolver + tool catalog) for the composition
        # root while depending on it like any other config-domain crate.
        "awaken-config-service",
        # Tenancy edge aspect (ADR-0051/0052): the opaque `ScopeId` the scope-keyed
        # tool catalog and the scoped config plane are keyed by.
        "awaken-tenancy",
        "awaken-run-ingress",
        # Extracted implementation owners injected behind established ports.
        "awaken-credential-materializer",
        "awaken-worker-transport-security",
        # dev-only: the real worker HTTP adapter runs the shared dispatch suite.
        "awaken-run-ingress-testkit",
        "awaken-run-executor-acp",
        "awaken-config-resolver",
        "awaken-credential-vault",
        "awaken-protocol-transport",
        "awaken-protocol-a2a",
        "async-trait",
        "axum",
        "serde",
        # SQL drivers still used by the legacy skill importer and Postgres commit
        # integration tests. Migration bundles belong to the extracted stores.
        "rusqlite",
        "sqlx",
        "tempfile",
        "base64",
        "form_urlencoded",
        "reqwest",
        "serde_json",
        "sha2",
        "regex",
        "zip", "thiserror",
        "tokio",
        "tracing", "uuid",  # unpredictable one-shot process-secret capability ids
    },
    # Exact credential materialization adapter extracted from Runtime Host.
    "awaken-credential-materializer": {
        "awaken-agent-contract", "awaken-credential-vault",
        "awaken-provisioning-contract", "awaken-run-executor-acp",
        "awaken-runtime-contract", "async-trait", "base64", "serde",
        "serde_json", "thiserror", "tokio", "uuid",
    },
    # Sole owner of Worker request authentication/signing and upstream identity.
    "awaken-worker-transport-security": {
        "awaken-run-ingress", "async-trait", "axum", "base64", "hmac",
        "reqwest", "serde", "serde_json", "sha2", "thiserror", "tokio",
    },
    "awaken-resource-reclaimer": {"awaken-resource-contract", "async-trait", "tokio"},
"awaken-resource-store": {"awaken-resource-contract", "awaken-scoped-migration", "awaken-scoped-migration-sqlite", "async-trait", "parking_lot", "proptest", "rusqlite", "serde", "serde_json", "sqlx", "tempfile", "tokio"},
    # Single-machine assembly binary: the composition root. Since the service
    # layer moved to awaken-runtime-host; it composes host/protocol/management router modes.
    # Test-only scenario host (Stage A): the mock models + build_*_router scenario
    # assemblies extracted from awaken-server. Depends on the product crate
    # for its now-pub assembly helpers + production executors.
    "awaken-scenario-host": {
        "async-nats",
        "async-trait",
        # Stage B2: the `management` scenario mode + brain-admin surface live in the
        # composition root; the config-router mode seeds the assistant via the
        # authoring plane. Dev-dep cycle-free (awaken-cli dev-depends back for mocks).
        "awaken-cli",
        "awaken-control",
        "awaken-admin-assistant",
        "awaken-admin-config-api",
        "awaken-agent-contract",
        "awaken-authz-enforce",
        "awaken-config-service",
        "awaken-config-resolver",
        "awaken-config-store",
        "awaken-executable-agent-catalog",
        "awaken-connection-plan",
        "awaken-credential-vault",
        "awaken-data-subject",
        # dev-only: capability-inventory tests use the canonical Resource adapter.
        "awaken-resource-store",
        "awaken-ext-builtin-tools",
        "awaken-ext-skills",
        "awaken-ext-mcp",
        "awaken-iam-contract",
        "awaken-iam-core",
        "awaken-iam-host",
        "awaken-iam-preset",
        "awaken-iam-server",
        "awaken-memory-store",
        "awaken-resource-store",
        "awaken-model-catalog",
        # Worker-fleet E2E reuses the production registration/heartbeat/drain
        # lifecycle and injects only a deterministic executor provider.
        "awaken-worker",
        "awaken-worker-transport-security",
        "awaken-observability",
        "awaken-protocol-a2a",
        # The A2A remote-delegate adapter the delegate-remote scenario injects.
        "awaken-run-executor-a2a",
        "awaken-protocol-ag-ui",
        "awaken-protocol-ai-sdk",
        "awaken-protocol-managed",
        "awaken-protocol-transport",
        "awaken-provider-genai",
        "awaken-run-executor-acp",
        "awaken-run-ingress",
        "awaken-runtime",
        "awaken-runtime-contract",
        "awaken-runtime-host",
        "awaken-managed-routers",
        "awaken-sandbox-policy-store",
        "awaken-sandbox-local",
        "awaken-server",
        "awaken-tenancy",
        "awaken-tool-relay",
        "awaken-webhook",
        "awaken-webhook-managed",
        "axum",
        "base64",
        "bytes",
        "futures",
        "http-body-util",
        "rusqlite",
        "serde_json",
        "tempfile",
        "thiserror",
        "tokio",
        "tower",
    },
    "awaken-server": {
        "awaken-acp-contract",
        "awaken-scenario-host",
        # dev-only: transport conformance exercises dispatch + Worker registry.
        "awaken-run-ingress", "awaken-runtime-host",
        "awaken-config-service", "awaken-credential-materializer",
        "awaken-worker-transport-security",
        # Coordinator consumes Control's immutable registration projection through
        # the read port; it never imports Control authoring or its database.
        "awaken-executable-agent-contract",
        # Coordinator owns Deployment/DeploymentRun through the inward repository
        # contract; protocol-managed remains only its HTTP projection.
        "awaken-deployment-contract",
        "awaken-session-store",
        "awaken-session-contract", "awaken-resource-contract",
        # Outer composition owns Hand topology/relay; runtime-host exposes only APIs/SPIs.
        "awaken-connection-plan", "awaken-tool-relay", "awaken-worker-registry", "awaken-managed-routers",
        # dev-only: A2A loopback wraps mocks in the remote-Agent adapter.
        "awaken-run-executor-a2a",
        # ADR-0052: the management assistant's descriptors seed the scope-keyed tool
        # catalog, and its executables/SPIs are wired at assembly.
        "awaken-admin-assistant",
        # ADR-0051/0052: the opaque scope id the reserved-scope seeding is keyed by.
        "awaken-tenancy",
        # Dev-only: the private erasure boundary test uses the Coordinator store.
        "awaken-captured-content-store", "awaken-observability", "awaken-authz-enforce",
        "awaken-run-executor-acp", "awaken-acp-application", "awaken-protocol-acp",
        "awaken-provisioning-contract",
        "awaken-protocol-managed",
        "awaken-protocol-ai-sdk", "awaken-protocol-ag-ui", "awaken-protocol-a2a",
        # Explicit MCP egress adapter, mounted by the data plane only when a
        # dedicated bearer is configured. Same protocol-adapter direction as
        # AI SDK / AG-UI / A2A; it never reaches into the control plane.
        "awaken-protocol-mcp",
        "awaken-protocol-transport",
        "awaken-provider-genai", "awaken-ext-skills", "awaken-sandbox-local",
        "awaken-memory-store",
        # The data-plane composition root opens the embedded File/Memory/Skill/
        # lifecycle family once and injects only its ports into runtime-host.
        "awaken-file-store", "awaken-skill-store", "awaken-resource-store",
        # Composition-only adapter: runtime-host exposes MemoryRepository + MemoryMounter
        # ports; awaken-server installs the FUSE/copy implementation without
        # coupling the host substrate to the worker implementation crate.
        "awaken-sandbox-memoryd",
        "awaken-config-store",
        "awaken-config-resolver",
        "awaken-admin-config-api",
        "awaken-model-catalog",
        "awaken-credential-vault",
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-runtime",
        # Embedded management-plane IAM (ADR-0042/0043 P1): contract = the
        # id/scope/request vocabulary, core = the argon2id token directory +
        # minter + default-deny PolicySet evaluator, preset = the seeded
        # Anthropic role catalog as data. Deliberately NOT awaken-iam-server:
        # its mandatory rusqlite 0.40 cannot share the `links = "sqlite3"`
        # graph with awaken-scoped-migration's rusqlite ^0.32 sqlite shell,
        # so the assembly persists the token/binding rows itself.
        "awaken-iam-contract",
        "awaken-iam-core",
        "awaken-iam-preset",
        # SqlStore/migrations for the embedded authz rows (links-safe since ADR-0005).
        "awaken-iam-server",
        # dev-dep (ADR-0048): the iam-host assembly, validated in an integration
        # test (IamGate + auth_layer, zero-config Local mode) ahead of full adoption.
        "awaken-iam-host",
        # The webhook plane (ADR-0048 / S10): the lifecycle-sink bridge + CRUD router
        # over the neutral dispatcher/store. Env-gated by AWAKEN_WEBHOOK_DIR.
        "awaken-webhook",
        "awaken-webhook-managed",
        # The embedded IAM's durable token/binding rows under
        # <AWAKEN_MGMT_DIR>/iam.sqlite — the same rusqlite generation every
        # other sqlite store in the workspace uses.
        "rusqlite",
        "async-trait",
        # Awaken Cloud's grant/catalog client is an outbound control-plane HTTP
        # adapter owned by this outer composition crate; these are wire-only
        # dependencies, not provider SDK or runtime-domain dependencies.
        "reqwest",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        "axum",
        # The product HTTP adapter owns web-console static asset and SPA delivery;
        # the outer CLI only locates/builds the distribution and supplies its path.
        "tower-http",
        "tower",
        "http-body-util",
        # dev-only (mcp_sessions test): the VaultRefresher refresh-grant path +
        # awaken_ext_mcp::to_tool_id assertion + base64 for the §2.3.1 header.
        "awaken-ext-mcp",
        "base64",
        # dev-only: the restart-persistence test rebuilds the durable management
        # router over one tempdir across simulated process lifetimes.
        "tempfile",
    },
    # Authoring / authz plane (Stage B2): the management CRUD surfaces + the embedded
    # IAM guard, split out of awaken-server. A sibling of the awaken-server data plane —
    # the two never depend on each other; awaken-cli is the composition root that weaves
    # them. It names the Managed wire (protocol-managed) + host ports (runtime-host) +
    # the webhook bridge it constructs the authoring routers over.
    "awaken-control": {
        # The shared config-authoring plane (config service + routers + resolver +
        # tool catalog), extracted from awaken-runtime-host so control ⊥ execution:
        # control names this, NOT the data-plane host.
        "awaken-config-service",
        "awaken-admin-assistant",
        "awaken-executable-agent-contract",
        "awaken-tenancy",
        "awaken-authz-enforce",
        "awaken-protocol-managed",
        "awaken-config-store",
        "awaken-config-resolver",
        # The data-plane skill catalog port: the capability inventory lists skill ids.
        "awaken-skill-store",
        "awaken-admin-config-api",
        "awaken-model-catalog",
        "awaken-credential-vault",
        "awaken-data-subject",
        "awaken-resource-store",
        "awaken-iam-contract",
        "awaken-iam-server",
        "awaken-iam-core",
        "awaken-iam-preset",
        "awaken-iam-host",
        "awaken-webhook-managed",
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-session-contract",
        "awaken-executable-agent-catalog",
        "rusqlite",
        "async-trait",
        "serde",
        "serde_json",
        # Secret-free, fixed-size request-body fingerprints for the durable
        # management audit middleware. The body itself is never persisted.
        "sha2",
        "base64",
        "uuid",
        "thiserror",
        "tokio",
        "axum",
        # dev-only: the authz restart tests open a tempdir-backed iam.sqlite.
        "tempfile",
        # dev-only: the management_guard / mint-route tests stand up a real axum
        # router and drive it (tower util + response-body reading).
        "tower",
        "http-body-util",
    },
    # The single aggregated command + the single-machine composition root (Stage B2):
    # it weaves the authoring plane (awaken-control) and the data plane (awaken-server)
    # into one management router. A composition root, so it may name them all.
    "awaken-cli": {
        # Neutral HTTP DTOs shared with composed distributions. Keeping the
        # projection in Foundation avoids a second Awaken-only wire shape.
        "awaken-api-contract",
        "awaken-control",
        "awaken-server",
        "awaken-authz-enforce",
        # The Worker role delegates lifecycle to the production execution worker.
        "awaken-worker",
        "awaken-worker-transport-security",
        "awaken-runtime-host",
        "awaken-config-service",
        "awaken-credential-materializer",
        "awaken-env-store",
        "awaken-ext-builtin-tools",
        "awaken-run-ingress",
        "awaken-work-store",
        "awaken-sandbox-policy-store",
        "awaken-resource-store",
        "awaken-resource-reclaimer",
        # Process adapter joining Control-owned subscriptions/secrets to the
        # Coordinator-owned Session lifecycle outbox.
        "awaken-webhook-managed",
        "awaken-file-store", "awaken-memory-store", "awaken-resource-contract",
        "awaken-session-contract", "awaken-session-store",
        "awaken-executable-agent-contract",
        "awaken-executable-agent-catalog",
        "awaken-sandbox-memoryd",
        # The ACP executor: the composition root wires an `acp:*` backend into the Serve
        # host by config (AWAKEN_ACP_ARGV), which the runtime-host plane does not do itself.
        "awaken-run-executor-acp",
        # Trusted-host ACP discovery/binding application service shared with Flow.
        "awaken-acp-application",
        # Deployment Hand topology is composed into Runtime's neutral executor port.
        "awaken-connection-plan",
        "awaken-acp-contract",
        "awaken-protocol-acp",
        "awaken-observability",
        # The composition root registers the brain's active-streams connection-load
        # gauge on the global OTel meter after init (#4), so it names opentelemetry.
        "opentelemetry",
        "awaken-protocol-managed",
        "awaken-model-catalog",
        "awaken-credential-vault",
        "awaken-admin-config-api",
        "awaken-config-store",
        "awaken-config-resolver",
        # Composition is the only layer allowed to open both role-owned privacy
        # adapters; it injects ports and never shares their database handles.
        "awaken-data-subject",
        "awaken-captured-content-store",
        # The durable skill catalog, shared by the host and the capability inventory.
        "awaken-skill-store",
        "awaken-admin-assistant",
        "awaken-tenancy",
        "awaken-iam-client",
        "awaken-provider-genai",
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "async-trait",
        "axum",
        "tokio",
        # dev-only: the management integration tests moved here from awaken-server.
        "awaken-scenario-host",
        "awaken-runtime", "awaken-provisioning-contract", "awaken-worker-contract",
        "awaken-ext-mcp",
        "awaken-iam-contract",
        "awaken-iam-core",
        "rusqlite",
        "base64",
        "tempfile",
        "http-body-util",
        "tower",
        "serde_json", "serde", "toml",  # bootstrap TOML is decoded only at this root
        # Deployment-owned absolute URLs are validated once at this composition
        # root before they are projected to the browser.
        "url",
    },
    # The EXECUTION-plane binary (the sandbox side): the ACP stdio<->TCP bridge, and
    # (later slices) the hand tool-executor + memoryd sidecar. A leaf that names no
    # awaken domain crate — it runs INSIDE the sandbox, opposite the control plane, so
    # it must not import control/runtime crates. tokio-only for now.
    "awaken-sandbox": {
        "tokio",
        # `hand` role only (optional feature): the tool executor + its channel/transport.
        # All lower-layer worker/runtime crates — bin depends down, no cycle.
        "awaken-tool-relay",
        "awaken-ext-builtin-tools",
        "awaken-connection-plan",
        "async-nats",
        "futures",
        "serde_json",
        # `memoryd` role only (optional feature): the FUSE/copy memory sidecar + the
        # durable sqlite store it projects. Lower-layer worker/resource crates.
        "awaken-sandbox-memoryd",
        "awaken-memory-store",
        # dev-only: the hand-role test drives a real ToolCall through the hand;
        # the memoryd-role test seeds + asserts a sqlite-backed store.
        "awaken-runtime-contract",
    },
    # Authority-store-isolated Worker crate: lifecycle and neutral injected Host ports only.
    # Product store/server adapters are composed by awaken-cli.
    "awaken-worker": {
        "awaken-acp-contract",
        "awaken-runtime-host",
        "awaken-credential-materializer",
        "awaken-ext-builtin-tools",
        "awaken-sandbox-container",
        "awaken-session-contract",
        "awaken-worker-transport-security",
        "awaken-resource-contract",
        "awaken-worker-contract",
        "awaken-provisioning-contract",
        "getrandom",
        "tokio",
        # The cloud-native admin surface (ADR-0022 D7): an axum router serving
        # /livez /readyz /admin/drain + the process Prometheus scrape.
        "awaken-observability",
        "axum",
        "tower",
        "awaken-runtime-contract",
        "async-trait",
    },
}

NEUTRAL_CRATES = {
    "awaken-agent-contract",
    "awaken-runtime-contract",
    "awaken-runtime",
}

EXTENSION_CRATES = {
    "awaken-ext-builtin-tools",
    "awaken-ext-permission",
}

# Adapter crates may name an external SDK; they are not bound by the neutral
# vocabulary rules but still have an explicit dependency allowlist above.
ADAPTER_CRATES = {
    "awaken-provider-genai",
    "awaken-store-postgres",
}

FORBIDDEN_NEUTRAL_TERMS = {
    "managed",
}

BUILTIN_TOOL_IDS = {
    "bash",
    "read",
    "write",
    "edit",
    "glob",
    "grep",
    "web_fetch",
    "web_search",
    "send_message",
    "cancel_task",
    "recover_failed_messages",
    "agent_run",
}

FORBIDDEN_NEUTRAL_TYPE_NAMES = {
    "TypedTool": "use Tool for the preferred typed API and RawTool for the low-level adapter",
    "BackgroundTask": "use ScheduledAction, ResumeTicket, or durable run dispatch by authority",
    # Removed roles: the executing side implements ToolExecutor and runs the tool
    # in-process (ADR-0007); where a tool runs is not a separate runtime role.
    "ToolExecutionLocus": "removed role; ToolExecutor is the sole neutral tool port",
    "ExecutorAdapter": "removed role; the executing side implements ToolExecutor, not a runtime adapter",
    "ExecutionBackend": "retired (ADR-0007); tool execution is in-process and remote-agent execution awaits a future ADR",
}

# Phrases that must not appear in neutral crate source (case-insensitive,
# whole-word). Worker/sandbox placement is not a runtime-core concern and is
# never named by the runtime core.
FORBIDDEN_NEUTRAL_PHRASES = {
    "worker placement": "worker placement is not a runtime-core concern; runtime core never names it",
    "sandbox placement": "sandbox placement is not a runtime-core concern; runtime core never names it",
}

FORBIDDEN_NEUTRAL_SYMBOLS = {
    "AgentRun": "agent_run is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Bash": "bash is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "CancelTask": "cancel_task is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Edit": "edit is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Glob": "glob is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Grep": "grep is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Read": "read is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "RecoverFailedMessages": "recover_failed_messages is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "SendMessage": "send_message is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "WebFetch": "web_fetch is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "WebSearch": "web_search is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "Write": "write is a concrete builtin tool owned by awaken-ext-builtin-tools",
    "ConfigPublicationCoordinator": "config publication coordination stays outside runtime core",
    "RegistryCompiler": "registry compilation stays outside runtime core",
}

FORBIDDEN_NEUTRAL_IMPL_TRAITS = {
    "Tool": "neutral crates may define the Tool mechanism, but concrete implementations belong in extensions/adapters",
    "RawTool": "neutral crates may define the RawTool mechanism, but concrete implementations belong in extensions/adapters",
}

def check_dependencies() -> list[str]:
    return _crate_dependency_fitness.check_allowed_dependencies(
        repo_root=REPO_ROOT,
        manifest_paths=iter_crate_manifests(),
        load_manifest=load_manifest,
        allowed_deps=ALLOWED_DEPS,
    )


def compile_word_patterns(words: set[str] | dict[str, str], *, ignore_case: bool = False) -> dict[str, re.Pattern[str]]:
    flags = re.IGNORECASE if ignore_case else 0
    return {
        word: re.compile(rf"\b{re.escape(word)}\b", flags)
        for word in words
    }


def check_neutral_code_boundaries() -> list[str]:
    errors: list[str] = []
    term_re = compile_word_patterns(FORBIDDEN_NEUTRAL_TERMS, ignore_case=True)
    tool_re = {
        tool_id: re.compile(rf'"{re.escape(tool_id)}"')
        for tool_id in BUILTIN_TOOL_IDS
    }
    type_re = compile_word_patterns(FORBIDDEN_NEUTRAL_TYPE_NAMES)
    symbol_re = compile_word_patterns(FORBIDDEN_NEUTRAL_SYMBOLS)
    phrase_re = compile_word_patterns(FORBIDDEN_NEUTRAL_PHRASES, ignore_case=True)
    impl_re = {
        trait_name: re.compile(rf"\bimpl(?:\s*<[^>]+>)?\s+(?:[\w:<>]+\s+for\s+)?{re.escape(trait_name)}\s+for\b")
        for trait_name in FORBIDDEN_NEUTRAL_IMPL_TRAITS
    }

    for crate_name in NEUTRAL_CRATES:
        for path in text_files(crate_name):
            content = path.read_text(encoding="utf-8")
            rel = path.relative_to(REPO_ROOT)
            for term, pattern in term_re.items():
                if pattern.search(content):
                    errors.append(f"{rel}: neutral crate uses product term {term!r}")
            for tool_id, pattern in tool_re.items():
                if pattern.search(content):
                    errors.append(f"{rel}: concrete builtin tool id {tool_id!r} leaked into neutral crate")
            for type_name, pattern in type_re.items():
                if pattern.search(content):
                    errors.append(
                        f"{rel}: forbidden neutral type name {type_name!r}; "
                        f"{FORBIDDEN_NEUTRAL_TYPE_NAMES[type_name]}"
                    )
            for symbol, pattern in symbol_re.items():
                if pattern.search(content):
                    errors.append(f"{rel}: forbidden neutral symbol {symbol!r}; {FORBIDDEN_NEUTRAL_SYMBOLS[symbol]}")
            for phrase, pattern in phrase_re.items():
                if pattern.search(content):
                    errors.append(f"{rel}: forbidden neutral phrase {phrase!r}; {FORBIDDEN_NEUTRAL_PHRASES[phrase]}")
            for trait_name, pattern in impl_re.items():
                if pattern.search(content):
                    errors.append(
                        f"{rel}: concrete {trait_name} implementation in neutral crate; "
                        f"{FORBIDDEN_NEUTRAL_IMPL_TRAITS[trait_name]}"
                    )
    return errors


def check_builtin_tool_ownership() -> list[str]:
    errors: list[str] = []
    for crate_name in EXTENSION_CRATES:
        crate_dir = next(CRATES.glob(f"*/{crate_name}/Cargo.toml"), None)
        if crate_dir is None:
            errors.append(f"missing required extension crate {crate_name!r} for concrete builtin tool ids")
    return errors


def check_tests_are_not_arch_owners() -> list[str]:
    """Catch accidental arch-hook fixtures hidden outside the hook itself.

    Test crates may define fake tools, but production architecture ownership
    still lives in this hook and the design docs. This guard keeps future
    negative fixtures from being checked into neutral src paths by mistake.
    """

    errors: list[str] = []
    for crate_name in NEUTRAL_CRATES:
        manifest = next(CRATES.glob(f"*/{crate_name}/Cargo.toml"), None)
        if manifest is None:
            continue
        tests = manifest.parent / "tests"
        if not tests.exists():
            continue
        for path in sorted(tests.rglob("*.rs")):
            content = path.read_text(encoding="utf-8")
            if "TypedTool" in content:
                errors.append(
                    f"{path.relative_to(REPO_ROOT)}: tests should not normalize the forbidden TypedTool name"
                )
    return errors


# Plane-aligned bucket dependency rules. Edges point inward toward contract/kernel;
# a bucket may depend only on the buckets in its set. The two load-bearing
# invariants: the kernel is store-unaware (`runtime` ⊥ stores/resources, D1), and
# the isolated execution tier links no durable store (`worker` ⊥ stores/resources,
# A-G17 — it commits back through the coordinator port).
#   contract   → shared vocabulary + ports; depends on nothing
#   runtime    → the kernel (agent loop + ext plugins); contract only
#   stores     → commit/event backends (impl contract ports; runtime-contract types)
#   resources  → mountable agent-resource backends
#   worker     → isolated execution; kernel + contract only, NEVER a store/resource
#   control    → self-hosted config/vault/iam authoring plane
#   server     → resident daemon; consumes every substrate + worker(-contract) + control
#   bin        → composed deployables / harnesses
BUCKET_ALLOWED_DEPS = {
    "contract": {"contract"},
    "runtime": {"contract", "runtime"},
    "stores": {"contract", "runtime", "stores"},
    "resources": {"contract", "runtime", "resources"},
    # `resources` is the common foundation config DEFINES and worker/sandbox
    # MATERIALIZES (files, memory stores, skills). Worker may depend on it — its DB
    # backends are feature-gated (default = inmem/fs only), so the isolated exec tier
    # links no heavy store. Worker still may NOT depend on `stores` (the commit-log
    # tier, G13 authority) — that is the store A-G17 keeps out of the exec tier.
    "worker": {"contract", "runtime", "resources", "worker"},
    # The authoring plane's assembly crate (awaken-control, Stage B2) names the Managed
    # wire (protocol-managed) + host ports (runtime-host) + the webhook bridge — all in
    # the `server` bucket — because the management CRUD it assembles is expressed in
    # those types. It stays a sibling of the awaken-server data-plane bin: neither
    # depends on the other (the composition root, awaken-cli, weaves them).
    "control": {"contract", "runtime", "stores", "resources", "control", "server"},
    "server": {"contract", "runtime", "stores", "resources", "worker", "control", "server"},
    "bin": {
        "contract",
        "runtime",
        "stores",
        "resources",
        "server",
        "worker",
        "control",
        "bin",
    },
    # Dev tooling / harnesses / teaching examples (publish=false): composed like bin/,
    # so they may name any plane (they are composition roots for tests/examples). Kept
    # out of bin/ so the deployables bucket holds only awaken-cli + the worker daemon.
    "devtools": {
        "contract",
        "runtime",
        "stores",
        "resources",
        "server",
        "worker",
        "control",
        "bin",
        "devtools",
    },
}
# The neutral-core / crate-layout fitness rules (contract purity, protocol-leaf, god-hub
# ratchet — Phases 0.1 / 0.2 / 3) live in `_arch_fitness.py` (pure predicates + cause-
# effect selftests), imported and driven by `main()` over the parsed crate specs. Split
# out to keep this file under the 2000-line hard limit.

# NOTE: this repo is fully open source. The open/closed line is the REPOSITORY
# boundary — closed commercial capabilities (placement, sharded dispatch, multi-
# region, microVM sandbox, billing/license/oversight) live in the separate
# `awaken-cloud` repo, which composes these open crates and injects closed
# implementations at the open ports (compose-not-merge). There is therefore no
# in-repo "BuSL" tier and no open-bin closure check: everything here ships open.


def main() -> int:
    _arch_fitness.selftest()
    _coordinator_authority_fitness.selftest()
    _migration_fitness.selftest()
    _service_data_ownership_fitness.selftest()
    errors = (
        check_dependencies()
        + check_neutral_code_boundaries()
        + check_builtin_tool_ownership()
        + check_tests_are_not_arch_owners()
        + check_bucket_direction(BUCKET_ALLOWED_DEPS)
        + _resource_plane_fitness.check_all(REPO_ROOT, CRATES)
        + _runtime_secret_boundary.check_all(REPO_ROOT, CRATES)
        + _provider_env_fitness.check_all(REPO_ROOT, CRATES)
        + _arch_fitness.check_all(architecture_fitness_specs())
        + _coordinator_authority_fitness.check_all(REPO_ROOT, CRATES)
        + _migration_fitness.check_all(REPO_ROOT)
        + _service_data_ownership_fitness.check_all(REPO_ROOT)
    )
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("OK - crate boundaries hold.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
