#!/usr/bin/env python3
"""Enforce Awaken crate dependency, vocabulary, and core/extension boundaries."""

from __future__ import annotations

import re
import sys
import tomllib
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
CRATES = REPO_ROOT / "crates"

# Async runtime infrastructure (not domain or provider types) is permitted in
# neutral crates: async-trait makes the ports dyn-safe, tokio drives execution,
# tokio-util carries the cancellation token. A model/provider SDK such as genai
# is deliberately NOT in this set for any neutral crate; it lives only in the
# provider adapter so G2/G10 hold.
ALLOWED_DEPS: dict[str, set[str]] = {
    # zeroize backs RedactedString's zero-on-drop (ADR-0043); a leaf crypto-hygiene
    # primitive, not a model/provider SDK.
    "awaken-agent-contract": {"serde", "serde_json", "thiserror", "async-trait", "tokio", "zeroize"},
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
        # dev-only: guard middleware tests drive a minimal axum router.
        "tokio",
        "tower",
    },
    # Webhooks (ADR-0048 / S10): a protocol-neutral projection sink. Signing +
    # Signing, the event shape, and HTTP delivery only — storage-neutral. No dep on
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
        "awaken-protocol-managed",
        "awaken-authz-enforce",
        "awaken-webhook",
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
    # The minimal single-machine assembly (a `bin/` deployable). Everything in this
    # repo is open, so there is no licensing boundary here; this allowlist just keeps
    # the standalone binary lean — it composes the open runtime + protocol planes and
    # a durable Postgres store, but not the fuller management/authoring surface that
    # server-local carries.
    "awaken-standalone": {
        "awaken-runtime-host",
        "awaken-observability",
        "awaken-protocol-managed",
        "awaken-protocol-ai-sdk",
        "awaken-protocol-ag-ui",
        "awaken-protocol-a2a",
        "awaken-protocol-transport",
        "awaken-authz-enforce",
        "awaken-webhook-managed",
        "awaken-config-resolver",
        # Open webhook plane over in-memory stores: the SecretStore the whsec_ key is
        # sealed behind (standalone has no durable config-authoring plane).
        "awaken-credential-vault",
        "awaken-runtime-contract",
        "axum",
        "async-trait",
        "tokio",
        "tower",
    },
    # Tenancy as an edge aspect (ADR-0051): the opaque `ScopeId` + the pure
    # ingress reconciliation (`resolve_scope`). A foundation leaf, serde-only;
    # names no iam/store/wire — the `ScopeId → ScopeRef` ACL lives in the PDP
    # adapter, never here.
    "awaken-tenancy": {"serde"},
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
        # The neutral MetricsRecorder port (#2): the OTel-backed recorder impl
        # lives here beside the Meter it feeds. A foundation contract leaf — no
        # domain capability travels, only the structure-only metric vocabulary.
        "awaken-runtime-contract",
    },
    # Host-side credential vocabulary (Credential / AuthChallenge /
    # CredentialRefresher) shared by the outbound wire clients (awaken-ext-mcp,
    # awaken-protocol-a2a). A leaf like the contracts: names no wire, store, or
    # runtime type, so clients depend on it without pulling anything upward.
    "awaken-credential": {"async-trait"},
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
        # feature `postgres`: network-DB CatalogRepo backend over the same
        # `catalog` migration scope (ADR-0043); the SQL driver, as config-store.
        "sqlx",
        # dev-only: reopen-from-file persistence tests.
        "tempfile",
    },
    # Data-subject aggregate + consent grants + repo/resolver (ADR-0050): a
    # control/compliance-plane store peer to the credential vault; reuses the
    # neutral DataSubjectId/Purpose/DataSubjectResolver from runtime-contract.
    "awaken-data-subject": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "rusqlite",
        # `spawn_blocking` for the sqlite adapter's off-thread connection work.
        "tokio",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        # feature `postgres`: the PgDataSubjectRepo / PgCapturedContentStore backend.
        "sqlx",
        # dev-only: reopen-from-file persistence test.
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
    "awaken-config-resolver": {
        "awaken-agent-contract",
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
        # The neutral `Disposition` resilience taxonomy: the ops cooldown routes map
        # a credential-probe failure onto a retry/cool-down policy (E3-4).
        "awaken-runtime-contract",
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
        "serde",
        "serde_json",
        "thiserror",
        "async-trait",
        "tokio",
        "tokio-util",
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
        "tracing",
    },
    # In-memory reference store backend (ADR-0039 2.2): the neutral commit/read
    # ports over `RwLock`/`HashMap`, dependency-free apart from the contract.
    "awaken-store-inmem": {
        "awaken-agent-contract",
        "awaken-store-conformance",
        "async-trait",
        "serde_json",
        "tokio",
    },
    # Filesystem store backend (ADR-0039 2.3): a durable append-only commit log
    # with crash recovery, reusing the in-memory read model as its cache.
    "awaken-store-fs": {
        "awaken-agent-contract",
        "awaken-store-inmem",
        "awaken-store-conformance",
        "async-trait",
        "serde_json",
        "tokio",
    },
    # Trait-generic store conformance suite (ADR-0039 2.6): backend-agnostic
    # behavioural checks every store backend runs from its own test crate.
    "awaken-store-conformance": {
        "awaken-agent-contract",
        "serde_json",
    },
    # Dispatch / run-ingress contract (ADR-0039 2.1): the durable-dispatch port
    # surface, factored out of the host so backends can depend on it (G2).
    "awaken-run-ingress-contract": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "async-trait",
        "serde",
        "thiserror",
    },
    # Provisioning contract: the neutral, data-only sandbox vocabulary and ports
    # (SandboxProvider / Sandbox / prepare_environment / admission). Names no OS
    # mechanism, host path, wire, or runtime type, so concrete realizers (lexical /
    # namespace / container) depend on it without pulling anything upward (G2/G3).
    "awaken-provisioning-contract": {
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        # dev-only: a fake exercises the async ports in unit tests.
        "tokio",
    },
    # Content-addressed blob store (ADR-0041): the neutral file-store trait +
    # `content_id` (BLAKE3) + local backends (fs/in-mem). Network backends
    # (postgres/s3) are separate crates over the same trait. A leaf — names no
    # provider, runtime, or host-path type.
    "awaken-file-store": {
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
    # Durable memory persistence (resources plane): id-keyed byte store behind the
    # ADR-0038 memory_store family. A resources-plane store like awaken-data-subject —
    # a scoped-migration bundle over sqlite (+ an optional postgres sibling) — so the
    # host backs memory durability with it while the runtime stays store-unaware. It
    # names no runtime/host/provider type; only the storage stack.
    "awaken-memory-store": {
        "async-trait",
        "thiserror",
        "tokio",
        "rusqlite",
        # feature `postgres`: the multi-node MemoryBlobStore backend.
        "sqlx",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        # ADR-0053 path-addressed MemoryFs: content hashing + durable record format.
        "sha2",
        "serde",
        "serde_json",
        # dev-only: conformance + reopen-from-file persistence tests.
        "tempfile",
    },
    # ADR-0053: the write-through memory-store FUSE server. A `server`-bucket crate
    # (it depends on the resources-tier `MemoryFs` and links `fuser`), realizing the
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
    # Durable skill catalog (resources plane): a SKILL.md-per-skill store the host
    # serves delivered skills from. A resources-plane store like awaken-data-subject —
    # a scoped-migration bundle over sqlite (+ an optional postgres sibling); names no
    # runtime/host/ext type, so awaken-ext-skills stays store-unaware.
    "awaken-skill-store": {
        "async-trait",
        "thiserror",
        "tokio",
        "rusqlite",
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
    # consumes the SDK's async response stream, so `futures` (StreamExt) is
    # permitted here and nowhere else.
    "awaken-provider-genai": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "genai",
        "async-trait",
        "futures",
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
        "awaken-runtime",
        "awaken-ext-builtin-tools",
        "awaken-observability",
        "tracing",
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
        "thiserror",
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
    # Managed Agents protocol adapter: the anti-corruption boundary between the
    # public Anthropic wire and the neutral runtime. It owns the public DTOs and
    # the axum router, so it may name `axum`/`tokio-stream`; it depends only on the
    # agent-domain contract (for `Message`) and drives a `SessionRuntime` port, so
    # it constructs no runtime. It is a product adapter, not a neutral crate.
    "awaken-protocol-managed": {
        "awaken-agent-contract",
        # Tenancy edge aspect (ADR-0051): the opaque `ScopeId` bound by the
        # `ScopedRepo` decorator so session persistence is tenant-isolable.
        "awaken-tenancy",
        # Managed vault/credential front door (ADR-0043): the credential domain +
        # the ACL that maps the Anthropic wire ⇄ the neutral credential model.
        "awaken-credential-vault",
        "awaken-managed-bridge",
        # Neutral egress vocabulary: the environments ACL maps the Anthropic
        # `BetaEnvironment.networking` config onto `NetworkPolicy`, so the sandbox
        # egress decision is a shared neutral fact, not an inline string match.
        "awaken-provisioning-contract",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
        "axum",
        "tokio-stream",
        "tracing",
        "tower",
        "http-body-util",
        # dev-only: vault E2E resolves the entered credential through the resolver.
        "awaken-config-resolver",
        "awaken-model-catalog",
    },
    # ACP bridge + supervisor (ADR-0041 Slice 3): the anti-corruption boundary
    # between an opaque agent's protocol stream and the neutral runtime. It drives
    # an `AgentChannel` (the transport seam) + `ProcessHandle` and projects events
    # through the `RunEventSink` binding port; it constructs no store. Agents plane.
    "awaken-protocol-acp": {
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
        "awaken-agent-channel",
        "awaken-protocol-acp",
        "async-trait",
        "thiserror",
        "tokio",
        # dev-only: build a `plugin_config` JSON value in the compaction-window test.
        "serde_json",
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
    # `message:send` wire and the neutral runtime. Request/response (returns a
    # `Task`), so it needs no streaming; drives an `A2aRuntime` port and constructs
    # no runtime.
    "awaken-protocol-a2a": {
        "awaken-agent-contract",
        "awaken-credential",
        "awaken-protocol-transport",
        "async-trait",
        "serde",
        "serde_json",
        "thiserror",
        "tokio",
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
    # names the `mcp` SDK for wire types only, never a runtime or store type.
    "awaken-mcp-wire": {
        "async-trait",
        "serde_json",
        "tokio",
        "tokio-util",
        "mcp",
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
        # Local sandbox: per-environment path isolation. It wraps the built-in tools
    # (jailing their paths to an IsolatedRoot), so it depends on the extension it
    # wraps and the neutral tool port. The remote/container provider lives in a
    # distributed repo and plugs in through the `SandboxProvider` trait.
    # Worker tier (ADR-0041/ADR-0053): the isolated-execution sandbox provider. It
    # links NO durable store (A-G17) — it resolves mount bytes through the injected
    # `BlobSource` port and computes the BLAKE3 content id itself.
    "awaken-sandbox-local": {
        "awaken-runtime-contract",
        # Implements the neutral sandbox ports (ADR-0041): a LocalProvider over the
        # provisioning contract, alongside the pre-contract Environment surface.
        "awaken-provisioning-contract",
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
        # The container tier is tool-transparent: it hands the ACP bridge an
        # AgentChannel (network duplex) to the process-as-container agent.
        "awaken-agent-channel",
        "async-trait",
        "serde_json",
        "thiserror",
        # feature `connection`: awaken-connection establishes the remote AgentChannel
        # (TCP dial + reverse dial); tokio provides the net stack. Both optional.
        "awaken-connection",
        "tokio",
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
        "awaken-runtime",
        "async-trait",
        "serde",
        "serde_json",
        "tokio",
    },
    # Extracted managed-agents SERVICE layer: the protocol-neutral SharedHost +
    # the two port adapters (ManagedHost / ProtocolHost) + every host module.
    # server-local composes it; nothing below the agents bucket depends on it.
    "awaken-runtime-host": {
        "awaken-agent-contract",
        "awaken-runtime-contract",
        "awaken-runtime",
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
        "awaken-config-store",
        # Tenancy edge aspect (ADR-0051/0052): the opaque `ScopeId` the scope-keyed
        # tool catalog and the scoped config plane are keyed by.
        "awaken-tenancy",
        "awaken-run-ingress",
        "awaken-run-executor-acp",
        "awaken-config-resolver",
        "awaken-credential-vault",
        # ADR-0050: consent read/write router over the data-subject aggregate.
        "awaken-data-subject",
        "awaken-protocol-managed",
        "awaken-protocol-transport",
        "awaken-protocol-a2a",
        "async-trait",
        "axum",
        "serde",
        # Durable ManagedSessionRepository backends (sqlite + postgres) over the
        # `managed` scoped-migration bundle; the SQL drivers + the shared migrator.
        "rusqlite",
        "sqlx",
        "awaken-scoped-migration",
        "awaken-scoped-migration-sqlite",
        "tempfile",
        "base64",
        "form_urlencoded",
        "reqwest",
        "serde_json",
        "sha2",
        "regex",
        "thiserror",
        "tokio",
        "tracing",
    },
    # Single-machine assembly binary: the composition root. Since the service
    # layer moved to awaken-runtime-host, this bin only composes that host + the
    # protocol facades + the management plane (admin/vault/IAM) into router
    # modes; it names no runtime/ext/store crate directly. Nothing depends on it.
    "awaken-server-local": {
        "awaken-runtime-host",
        # ADR-0052: the management assistant's descriptors seed the scope-keyed tool
        # catalog, and its executables/ports are wired at assembly.
        "awaken-admin-assistant",
        # ADR-0051/0052: the opaque scope id the reserved-scope seeding is keyed by.
        "awaken-tenancy",
        # ADR-0050: the data-subject consent/erasure store backing the erasure endpoint.
        "awaken-data-subject",
        # Remote-hand scenario mode (ADR-0044): route a run's tools to a hand task.
        "awaken-tool-relay",
        "awaken-connection-plan",
        "awaken-ext-builtin-tools",
        # Relay topology (ADR-0045): the NATS broker transport for brain↔hand.
        "async-nats",
        "bytes",
        "futures",
        "awaken-observability",
        "awaken-authz-enforce",
        "awaken-run-executor-acp",
        "awaken-protocol-managed",
        "awaken-protocol-ai-sdk",
        "awaken-protocol-ag-ui",
        "awaken-protocol-a2a",
        "awaken-protocol-transport",
        "awaken-provider-genai",
        "awaken-memory-store",
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
        "serde_json",
        "thiserror",
        "tokio",
        "axum",
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
    "BackgroundTask": "use ScheduledAction, RunWaitingState, or durable run dispatch by authority",
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


def load_manifest(path: Path) -> dict:
    with path.open("rb") as handle:
        return tomllib.load(handle)


def package_name(manifest: dict) -> str:
    return str(manifest["package"]["name"])


def dependency_names(manifest: dict) -> set[str]:
    deps: set[str] = set()
    for section in ("dependencies", "dev-dependencies", "build-dependencies"):
        deps.update(manifest.get(section, {}).keys())
    return deps


def iter_crate_manifests() -> list[Path]:
    if not CRATES.exists():
        return []
    # Crates are grouped by product bucket: crates/<runtime|agents>/<crate>/Cargo.toml.
    return sorted(CRATES.glob("*/*/Cargo.toml"))


def check_dependencies() -> list[str]:
    errors: list[str] = []
    for manifest_path in iter_crate_manifests():
        manifest = load_manifest(manifest_path)
        name = package_name(manifest)
        allowed = ALLOWED_DEPS.get(name)
        if allowed is None:
            errors.append(f"{manifest_path.relative_to(REPO_ROOT)}: unknown crate boundary")
            continue

        unexpected = dependency_names(manifest) - allowed
        if unexpected:
            errors.append(
                f"{manifest_path.relative_to(REPO_ROOT)}: disallowed dependencies: "
                + ", ".join(sorted(unexpected))
            )
    return errors


def text_files(crate_name: str) -> list[Path]:
    # Resolve the crate under its product bucket (crates/<bucket>/<crate>/src).
    manifest = next(CRATES.glob(f"*/{crate_name}/Cargo.toml"), None)
    if manifest is None:
        return []
    src = manifest.parent / "src"
    if not src.exists():
        return []
    return sorted(path for path in src.rglob("*.rs") if path.is_file())


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
    "control": {"contract", "runtime", "stores", "resources", "control"},
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
}


def normal_dependency_names(manifest: dict) -> set[str]:
    """Normal + build deps only. Dev-dependencies are test/example wiring and may
    cross buckets freely (a test is a composition root)."""
    deps: set[str] = set()
    for section in ("dependencies", "build-dependencies"):
        deps.update(manifest.get(section, {}).keys())
    return deps


def check_bucket_direction() -> list[str]:
    """The product-bucket order (see BUCKET_ALLOWED_DEPS). provisioning/config/
    resources are runtime-independent; the runtime never depends on config or
    agents. Keys off the actual directory a crate lives in, so it stays correct as
    crates are added without touching any allowlist. Dev-deps are exempt."""
    errors: list[str] = []
    bucket: dict[str, str] = {}
    for manifest_path in iter_crate_manifests():
        name = package_name(load_manifest(manifest_path))
        bucket[name] = manifest_path.parent.parent.name  # crates/<bucket>/<crate>
    for manifest_path in iter_crate_manifests():
        manifest = load_manifest(manifest_path)
        name = package_name(manifest)
        allowed = BUCKET_ALLOWED_DEPS.get(bucket.get(name), set())
        for dep in normal_dependency_names(manifest):
            dep_bucket = bucket.get(dep)
            if dep_bucket is None or dep_bucket in allowed:
                continue
            errors.append(
                f"{bucket.get(name)}/{name} depends on {dep_bucket}/{dep} "
                f"(a {bucket.get(name)} crate may depend only on {sorted(allowed)})"
            )
    return errors


def check_runtime_is_secret_resolution_free() -> list[str]:
    """D6/D9 (ADR-0043): the runtime and its extensions receive an already-resolved
    secret value (`RedactedString`) only — never a handle, a resolver, or a vault
    ref. So the secret-*lifecycle* vocabulary must not appear anywhere under
    crates/runtime/. `RedactedString` itself is fine (it is the resolved value)."""
    banned = ("SecretHandle", "SecretResolver", "SecretStore", "CredentialBinding", "SecretRef")
    errors: list[str] = []
    runtime_dir = CRATES / "runtime"
    if not runtime_dir.exists():
        return errors
    for path in runtime_dir.glob("**/*.rs"):
        text = path.read_text(encoding="utf-8")
        for token in banned:
            if token in text:
                errors.append(
                    f"{path.relative_to(REPO_ROOT)}: runtime must be secret-resolution-free "
                    f"(D6/D9): found `{token}` — resolution lives in the host, not the runtime"
                )
    return errors


# NOTE: this repo is fully open source. The open/closed line is the REPOSITORY
# boundary — closed commercial capabilities (placement, sharded dispatch, multi-
# region, microVM sandbox, billing/license/oversight) live in the separate
# `awaken-cloud` repo, which composes these open crates and injects closed
# implementations at the open ports (compose-not-merge). There is therefore no
# in-repo "BuSL" tier and no open-bin closure check: everything here ships open.


def main() -> int:
    errors = (
        check_dependencies()
        + check_neutral_code_boundaries()
        + check_builtin_tool_ownership()
        + check_tests_are_not_arch_owners()
        + check_bucket_direction()
        + check_runtime_is_secret_resolution_free()
    )
    if errors:
        for error in errors:
            print(f"ERROR: {error}", file=sys.stderr)
        return 1
    print("OK - crate boundaries hold.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
