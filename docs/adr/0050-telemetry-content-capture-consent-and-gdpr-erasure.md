# ADR-0050: Telemetry/Eval Content Capture — One Capture Decision, Consent as a Trust-Grant, GDPR Erasure by Subject

- Status: Proposed
- Date: 2026-07-10
- Builds on:
  [ADR-0048](0048-iam-host-adoption-org-workspace-path-alignment-and-a2a-carve-out.md)
  and [ADR-0042](0042-public-api-tenancy-authz-and-front-door-consistency.md)
  (Org → Workspace tenancy; Org is the tenant/controller root — an Access-context
  fact owned by `awaken-iam`, per [ADR-0051](0051-tenancy-edge-aspect-one-opaque-scope-id.md) D7,
  which retired the orphan in-repo `awaken-scope` crate),
  [ADR-0043](0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md)
  (the runtime-unaware secret seam — extended here to privacy),
  [ADR-0047](0047-compaction-as-agent-run-and-the-context-plane-boundary.md)
  (the context-plane boundary that telemetry observes)
- Relates to: [ADR-0049](0049-a2a-cross-tenant-federation-carve-out.md)
  (cross-Org isolation), the observability tracing landing (OTel GenAI spans),
  and the planned `awaken-eval` and observability-metrics crates (goal-gap P0)

## Context

Two planned/existing capabilities record **conversation content** — prompts,
completions, tool arguments/results, full replayed dialogues:

- **Tracing / observability** — GenAI spans already exist; the metrics/trace-store
  layer is planned. Today tracing is *off unless a sink is configured*
  (`AWAKEN_TRACE_FILE` / `OTEL_EXPORTER_OTLP_ENDPOINT`; `OtelConfig::is_configured()`),
  and GenAI spans do **not** capture message content yet.
- **Eval** (`awaken-eval`, not yet built) — fixture replay records **complete
  conversations by design**; a fixture store is a data-subject-bearing corpus.

Conversation content is **personal data** under GDPR: it can contain the end
user's PII anywhere in free text. The only redaction that exists today is
credential-scoped and schema-driven (`awaken-ext-mcp/src/sensitive.rs`
`redact_arguments` over `x-sensitive`/`writeOnly`/`format:password` fields) — it
does not cover conversation content or unknown-position PII.

Three domain facts frame the design:

1. **`Org` is the tenant** — the tenant / billing / partition root of the scope
   tree, an Access-context fact owned by `awaken-iam` (ADR-0051 D7; the in-repo
   `awaken-scope` crate that once modelled this was an orphan and is retired).
   Tenancy is strictly `Org ⊃ Workspace` (no Project tier). The GDPR **data
   controller** boundary, the isolation/residency partition, and billing all
   coincide with **Org**.
   Workspace/Agent are operational subdivisions *inside* one controller.
2. **The runtime must stay tenancy- and secret-unaware** (ADR-0043; enforced by
   `check_runtime_is_secret_resolution_free`). Privacy must not become a new leak
   of scope/consent into the execution kernel.
3. **We are usually the processor, not the controller.** In B2B deployments the
   Org (our customer) obtained consent from *its* end users. We receive a consent
   **assertion**, we do not run the consent UX ourselves — but we must honour and
   retain it.

The GDPR obligations that bear on code: **data minimisation & by-design defaults**
(Art. 5/25), **purpose limitation** (Art. 5), **storage limitation / retention**
(Art. 5(e)), **right to erasure** (Art. 17), **right of access** (Art. 15),
**data residency** (Chapter V).

## Decision

### D1: Telemetry is two data classes; only *content* is the GDPR surface

Split every telemetry/eval emission into **structure** (span tree, latency, token
counts, model id, stop reason, error code — generally *not* personal data) and
**content** (prompt/completion text, tool args/results, replayed dialogue —
personal data). The knob is a **total-ordered lattice**, not a bool:

```
ContentCapture:   Off  ⊏  Structured  ⊏  Full
                  ─────    ──────────    ────
                  nothing  spans w/o     spans + content
                           content       (content still redacted, see D6)
```

`Structured` is the safe operational default; `Off` and `Full` are the extremes.
`meet` (greatest lower bound = the *stricter* value) is the only combinator used
below. This aligns with the OTel convention (`OTEL_INSTRUMENTATION_GENAI_CAPTURE_
MESSAGE_CONTENT`, default false) rather than inventing our own vocabulary.

### D2: One invariant governs everything — a single `CaptureDecision`

For any recording, the effective capture level is the `meet` of four *config*
inputs and one *consent* gate:

```
CaptureDecision.level =
    meet( org_ceiling,          # D3 — legal ceiling (the only binding one)
          workspace_narrow,     # operational narrowing
          agent_narrow,         # operational narrowing
          session_request,      # operational narrowing
          consent_cap(subject, purpose) )   # D4 — subject gate

reason ∈ { ok | clamped_by_ceiling | clamped_by_request | no_consent }
```

Because `meet` is monotone over a total order, **a lower layer can only tighten,
never widen** — the "downstream can't exceed the tenant ceiling" rule is a
property of the lattice, not of validation code. There is no other business rule;
everything else in this ADR is where the inputs live and how the output is
carried, redacted, and erased.

### D3: `Org` carries the *compliance* ceiling; lower layers are *operational* narrowing

The four config layers are **not peers** — they differ in kind:

| Layer | Kind | Authority |
| --- | --- | --- |
| **Org** | **Compliance baseline** (controller) | `content_capture`, `redaction`, `retention_days`, `residency` are **required**; this is the legally binding ceiling and the erasure/isolation partition |
| Workspace | Operational | optional; may only make the value **stricter** |
| Agent | Operational | optional; may only make the value **stricter** |
| Session | Operational (per D5/D8) | optional; may only make the value **stricter** |

`retention` composes by `min` (shorter wins); `content_capture`/`redaction`
compose by "stricter wins". Residency and the erasure/TTL partition are **Org-keyed**
and not narrowable below Org (a workspace cannot move data to another region).
This makes the Org aggregate the single home of controller semantics and keeps
the lower three purely operational — the two decision kinds never mix.

### D4: Consent lives on the `DataSubject` aggregate — as an Awaken-neutral grant, not the Anthropic `trust_grants` wire field (`UserProfile` projects the same subject)

`UserProfile` already models the data subject (`awaken-protocol-managed`), so
consent belongs there — **no `ConsentRecord` type, no new aggregate**. But two
facts constrain *how*, verified 2026-07-10:

- **The end-user (subject) IS Anthropic's model, in a *separate* beta.** `UserProfile`
  mirrors `@anthropic-ai/sdk beta.userProfiles.*` — verified against SDK v0.105.0
  `resources/beta/user-profiles.d.ts`. It is a **real official beta but gated by its
  own header `user-profiles-2026-03-24`, distinct from `managed-agents-2026-04-01`**
  (they are parallel betas under `client.beta`). `relationship` (external/resold/
  internal) anchors the subject to "the platform that owns the API key" (the
  developer). So the subject layer is **not** our invention; our increment is only
  the consent/erasure/session-association on top of it.
- **`trust_grants` is a CLOSED shape and cannot hold consent.** Authoritative type:
  `trust_grants: { [grantName]: { status: 'active'|'pending'|'rejected' } }` — the
  end-user's OAuth-style authorization grants, established via the enrollment URL,
  not GDPR telemetry consent. A consent record does not fit `{status}`; overloading
  it is a *type* violation, not just a semantic risk.
- **The former implementation was a stub.** It hardcoded `trust_grants`, omitted
  update support, minted an unsigned fixed-horizon enrollment receipt, and used
  the Managed Agents beta header. The implemented path now projects the sole
  `DataSubject` aggregate, accepts trust-grant updates, uses a separately derived
  HMAC key for expiring enrollment tokens, and enforces
  `user-profiles-2026-03-24`.

Therefore consent is an **Awaken-neutral grant on the subject**, kept **off** the
Anthropic `trust_grants` projection (a distinct consent sub-resource — `{status}`
leaves no room to piggyback). The `enrollment_url` *flow* is the right,
Anthropic-compatible collection mechanism, but must be built end-to-end (no stub).
Consent is then a grant keyed by purpose. The application and its store adapter
are separate crates, but there remains one aggregate, one repository port, and
one durable write path:

- **Purposes are exactly two**: `telemetry_content`, `eval_recording`. Each is
  independently granted, independently retained, independently withdrawn (purpose
  limitation). We deliberately do **not** build a purpose taxonomy.
- **Two entry paths, both already exist**:
  - *End-user enrollment* — `POST /v1/user_profiles/:id/enrollment_url` → the
    subject grants → lands in `trust_grants["telemetry_content"]`.
  - *Controller assertion (processor model)* — `POST /v1/user_profiles/:id` with
    a `trust_grants` patch; the Org asserts consent on its subject's behalf.
- **Absence ⇒ cap at `Structured`.** `consent_cap` returns `Full` only when a
  live grant for that purpose exists; otherwise `Structured` (never blocks
  structure telemetry, always blocks content without consent).
- **Withdrawal** = grant → false / key removed ⇒ stop capture **and** trigger
  erasure (D7).

### D5: Resolution happens at the boundary; the runtime sees only the resolved primitive

All five inputs are resolved in the **config/host plane** (the config-resolver +
runtime-host, which already touch scope, snapshot, and profiles). What crosses
into the runtime and the sinks is a **single opaque primitive**:

```rust
// foundation/awaken-runtime-contract — runtime may depend on this
pub struct CaptureDecision {
    pub level: ContentCapture,               // Off | Structured | Full
    pub redactor: Arc<dyn ContentRedactor>,  // D6
}
pub enum ContentCapture { Off, Structured, Full }
```

The runtime and sinks **never import** `awaken-tenancy`, `user_profile`, or consent
types — they receive an enum and a function. This extends ADR-0043's
runtime-unaware seam to privacy for free: the existing
`check_runtime_is_secret_resolution_free` boundary already forbids the runtime
from reaching into tenancy, so the privacy decision *must* arrive pre-resolved.

### D6: `ContentRedactor` is a port, orthogonal to the level, shipped with two real impls

Level decides *which fields* are recorded; the redactor decides *how thoroughly
the recorded text is scrubbed*. `Full` ≠ raw — the common middle ground is
"record the dialogue but scrub emails/cards".

```rust
pub trait ContentRedactor {
    fn redact(&self, kind: ContentKind, text: &str) -> Cow<'_, str>;
}
```

- **Two implementations ship together** (no stub, per the no-stubs rule):
  `NoopRedactor` (open/single-machine) and `RegexPiiRedactor` (email/phone/card/
  gov-id). A `DlpRedactor` is added only when a real DLP backend is wired.
- The existing schema-driven `redact_arguments` becomes **one input source** the
  redactor composes with (known sensitive fields) *plus* text-level PII scanning
  (unknown-position PII). Redaction runs **write-side, before** content reaches a
  span attribute / fixture / event-history projection; the real content still
  flows through the runtime to the model — only the persisted projection is
  scrubbed. The redactor never runs on the inference hot path.
- The redactor is a **stateless domain service** placed in
  `foundation/awaken-observability` — a trait + impls, **not a new crate**.

### D7: Erasure & retention are subject-keyed store capabilities, partitioned by Org

Every persisted **content** record (trace, eval fixture, content-bearing event)
carries `data_subject_id` + `purpose` + `retention` (from D3). Stores implement:

```rust
pub trait DataSubjectErasure { fn erase(&self, id: &DataSubjectId) -> Result<ErasureReceipt>; }
```

- **Erasure** (Art. 17) fans `erase` across trace + eval + memory +
  session content stores, **within the subject's Org partition**; cross-Org is
  strictly isolated (consistent with ADR-0049 cross-tenant carve-out).
- **Retention** (Art. 5(e)) = per-Org TTL sweep; `Structured`-only records need
  no subject key.
- The current `AWAKEN_TRACE_FILE` append-only sink has no subject key and no TTL
  → it is declared **`Structured`-only** (never `Full`) until it gains erasure,
  or replaced by a subject-keyed trace store. This is a hard constraint, not a
  preference.

### D8: API surface — fields on owning aggregates + one read-only decision projection

No new endpoint tree. Each concern lands on the aggregate that owns it:

- **Ceiling** — a `telemetry` block PATCHed onto existing config aggregates
  (`/v1/config/...`), required at Org, optional-and-only-stricter below.
- **Consent** — an Awaken-neutral consent grant on the `user_profile` subject
  (D4), collected via the enrollment flow; **not** the Anthropic `trust_grants`
  field (closed `{status}` shape).
- **Subject attribution** — follows Anthropic's grain: **per-request
  `user_profile_id`** on the Messages create params ("attribute this request to…;
  use when acting on behalf of a party other than your organization" — SDK
  `MessageCreateParamsBase`). **Anthropic has NO `session.user_profile_id`**;
  sessions bind to agent + environment + `vault_ids` only. A session-level
  `user_profile_id` (as a default attribution for its runs) is therefore an
  **Awaken extension, not a Managed Agents contract**, and must be labelled as
  such. Managed Session event JSON remains exact and rejects that field; Awaken
  carries request-grain attribution in `anthropic-user-profile-id`, the same
  transport header to which the official Messages SDK projects
  `user_profile_id`, and maps it to neutral `DataSubjectId` at the adapter edge.
- **Request** — a typed `content_capture` field on `POST /v1/sessions` and the
  existing `POST /v1/sessions/:id` patch. It is a **field, not a `metadata` key**
  (metadata stays for opaque business tags).
- **Decision (read-only projection)** — `GET /v1/sessions/:id` reflects
  `content_capture: { requested, ceiling, consent, effective, reason }`. GDPR
  auditability ("why was this captured?") is a response field, not a log dig.
- **Erasure / access** — `POST /v1/user_profiles/:id/erasure` (Art. 17) and
  optionally `GET /v1/user_profiles/:id/data` (Art. 15), both subject-scoped.

### D9: Assembly matrix — the design collapses to a two-item core

Only two things are unconditional; the other three are **compile-time absent**
when their dimension is absent (this is the test of the design's simplicity):

| Piece | `standalone` (single-tenant, zero-BuSL) | `server-local` (managed) |
| --- | --- | --- |
| Content/Structure split + `CaptureDecision` | ✅ (from env default) | ✅ |
| Subject-keyed erasure + TTL (`DataSubjectErasure`) | ✅ | ✅ |
| Org→Workspace ceiling chain | ✗ (no config plane) | ✅ |
| Consent grants + enrollment (`awaken-data-subject-application`) | ✗ (not assembled) | ✅ |
| `RegexPiiRedactor`/`DlpRedactor` | Noop default | ✅ |

In `standalone`, `CaptureDecision.level` = env default (`AWAKEN_TRACE_FILE` ⇒
`Structured`; OTel capture var ⇒ `Full`) and the redactor is `Noop`. The three
managed pieces are not stubbed-to-true — they are not assembled.

### D10a: The subject is a neutral `DataSubject` — opaque id + resolver port; protocols project it

The party a request is attributed to is a **neutral-core** concept (attribution +
consent + erasure apply to every protocol, per ADR-0034 "protocol is a projection
over the neutral core"), so it does **not** live inside `awaken-protocol-managed`.

`DataSubject` is a standalone **entity**; a run/message/record merely **references**
it by id (`data_subject_id`). There is no `RunDataSubject` type — the `Run` qualifier
would name a *relationship* (a run references a subject), not a kind of entity, and a
reference is just a field. Attribution is per-message (Anthropic puts `user_profile_id`
on the message), so a run may reference more than one subject over its lifetime; the
reference handles 1:1 and per-message identically. Two pieces, no more:

```rust
// neutral core — carried on the request; opaque, no attributes, no PII
pub struct DataSubjectId(String);

// the ONE customization seam (this is the trait, not the subject value)
pub trait DataSubjectResolver: Send + Sync {
    fn consent(&self, id: &DataSubjectId, purpose: Purpose) -> ConsentStatus;
    fn erase(&self, id: &DataSubjectId) -> ErasureReceipt;   // D7
}
```

- **External passes only the opaque `DataSubjectId`, per request** — the neutral
  analog of Anthropic's per-message `user_profile_id` ("attribute this request to…").
  There is **no `session.user_profile_id`**; a session-level default is an Awaken
  extension (D8). The runtime/telemetry hot path holds only the opaque id — never the
  subject's attributes — so data-minimisation and the runtime-unaware seam (D5) hold.
- **The subject value is NOT a trait; the pluggability is the resolver.** What varies
  is *where subject facts come from*, not what a subject *is*. One opaque value + **two**
  shipped resolvers — `NullResolver` (standalone: no tracking; erase = delete tagged
  records by id) and `RepoDataSubjectResolver` (managed: reads the
  `awaken-data-subject-application` aggregate, which `UserProfile` also projects).
  An `ExternalResolver` (delegating
  consent to the developer's own system) is a **documented future**, not built now — no
  speculative impls (D11).
- **`consent` is resolved once at run start** (config/host boundary, D5), not per
  token — the resolver is off the inference loop.
- **Managed `UserProfile` is one projection** of this subject (its `relationship`/
  `trust_grants`/`enrollment_url` are managed-wire detail, not core); other front
  doors (ai-sdk/ag-ui/a2a/acp) project their own end-user vocabulary to
  `DataSubjectId`, or none.

### D10b: Naming

- `ContentCapture` = the config **enum**; `CaptureDecision` = the **resolved**
  value (a decision, not a policy). The two states are never conflated under one
  `Policy` name.
- **The entity is `DataSubject` / `DataSubjectId`; runs/records carry a
  `data_subject_id` reference; the port is `DataSubjectResolver`.** Chosen over bare
  `Subject` (ambiguous against the three "user"-like actors: developer, IAM principal,
  end user) and over `User` (wrong for non-person attribution). The GDPR term is
  precise and unambiguous *in this bounded context*, so no qualifier is needed; a
  `Run` prefix (`RunDataSubject`) was rejected as redundant — it names the reference
  edge, not the entity. `relationship` (external/resold/internal) is a
  **managed-projection** field, not a core property, so it does not bear on the name.
- No `Manager`/`Service`/`Handler` suffixes introduced. `ContentRedactor`,
  `DataSubjectErasure`, `ErasureReceipt`, `ContentKind` name their role.
- Wire vocabulary follows OTel/GDPR terms (`content_capture`, `redaction`,
  `retention`, `consent`, `purpose`, `erasure`), not invented words.

### D11: Pinned implementation seams (nothing left to design ad hoc)

**Consent grant shape (minimal-complete).** Withdrawal never deletes the grant
(audit proof of when consent existed/ended); it flips status and triggers content
erasure.

```rust
enum Purpose { TelemetryContent, EvalRecording }            // closed
enum LawfulBasis { Consent, Contract, LegitimateInterest }  // default Consent
enum ConsentStatus { Granted, Pending, Withdrawn }
struct ConsentGrant { purpose: Purpose, status: ConsentStatus,
                      basis: LawfulBasis, granted_at: Timestamp, version: String }
// mapping: Granted for the purpose ⇒ Full allowed; else ⇒ capped at Structured
```

**`DataSubject` aggregate + home.** Crate
`control/awaken-data-subject-application` owns the Org-scoped aggregate,
commands, queries, and repository port. Crate
`stores/awaken-data-subject-store` owns only SQLite/PostgreSQL adapters; its
in-memory adapter is test-support only.

```rust
struct DataSubject { id: DataSubjectId /* dsub_ */, org: OrgId,
                     external_id: Option<String>, consents: Vec<ConsentGrant>,
                     created_at: Timestamp, updated_at: Timestamp }
// invariants: belongs to exactly one Org (controller); ≤1 active grant per purpose
// (a new grant supersedes, history retained); consents are part of THIS aggregate.
trait DataSubjectRepo { /* create/get/list + revision-fenced CAS */ }  // app port
```

The Managed `UserProfile` routes in `awaken-protocol-managed` read/write this
aggregate and project it to the Anthropic wire shape. The unified protocol crate
owns only wire DTOs, beta enforcement, routing, and projection—not business
state. One store, one aggregate.

**Erasure checkpoint concurrency.** The process-manager row has its own strict
revision and is updated only through compare-and-swap. A content eraser is
idempotent by subject and replays its durable receipt. Therefore two Control
replicas may repeat the physical call across an effect/checkpoint crash window,
but only one checkpoint counts it; the loser reloads the winning target set and
receipt before continuing. Blind checkpoint upsert is forbidden.

**Enrollment flow (no stub).** standalone: routes not mounted, no consent. managed:
(1) `POST /v1/user_profiles/:id/enrollment_url` mints a **signed, expiring** URL
`/enroll/<token>` whose token carries `{data_subject_id, org, purposes, expires_at}`
HMAC-signed with a server key (no server-side pending state); (2) developer sends it
to the end user; (3) end user opens a **minimal server-rendered consent page** (not an
SPA) listing the purposes; (4) accept → `POST /enroll/<token>/grant` writes
`ConsentGrant{Granted, version, granted_at}`, decline → `Withdrawn`; (5) optional
webhook callback to the developer reuses existing webhook infra (follow-on). v1
no-stub minimum = mint → page → grant recorded.

**Resolution cadence.** `CaptureDecision` is resolved at **each run/turn boundary**
in `awaken-runtime-host` (never per-token, never once-per-session): meet(ceiling ×
request × consent) + build the redactor from the resolved redaction mode → hand
`{level, redactor}` + opaque `data_subject_id` to the runtime for that turn. Withdrawal
gives a **two-part guarantee**: content erasure is immediate (`erase` by id); capture
stops at the **next turn** (best-effort-prompt, seconds). One turn's tokens use one
decision.

## How this satisfies simple design & DDD

**Simple design (structure clear, features intact — not features cut):**

- **One rule, one lattice.** The entire behaviour is `meet` over
  `Off ⊏ Structured ⊏ Full`. "Downstream can't widen" is a lattice property, so
  there is no scatter of guard code. Complexity is *collapsed into one primitive*
  (`CaptureDecision`), not spread across layers.
- **Transparency is structural.** The decision projection (D8) exposes the `meet`
  inputs and the reason code; no side-channel is needed to explain an outcome.
- **It degrades to a two-item core (D9).** The single-machine build carries only
  the irreducible obligations (content split, erasure); the multi-tenant/consent
  machinery is absent, not stubbed. This is the discriminator that keeps the
  design elegant rather than a tax on `standalone`.

**DDD best practices:**

- **One aggregate; persistence follows service ownership.** Consent = an Awaken-neutral grant on
  the existing `user_profile` subject aggregate (D4 — a net-new write+store path,
  but not a second aggregate); captured runtime content is not part of that
  aggregate and therefore lives in the Coordinator-owned
  `awaken-captured-content-store`; ceiling = fields on existing
  config/Org aggregates (D3); redactor = a stateless domain service in an existing
  crate (D6); Coordinator erasure composes the existing captured-content and
  portable ACP session adapters behind one stable target (D7). This directly avoids
  the anemic-leaf-crate smell called out in the goal-vs-dev gap review.
- **Bounded contexts respected; contexts don't leak.** Controller/compliance
  semantics live only on the Org aggregate; operational narrowing lives on
  workspace/agent/session; the runtime stays privacy-unaware behind
  `CaptureDecision` (D5), extending — not duplicating — ADR-0043's invariant.
- **Anti-corruption at the processor edge.** Consent arrives as an *assertion* at
  the managed API boundary (D4), an explicit ACL between controller (Org) and
  processor (us); we never assume the controller's consent UX.
- **Ubiquitous language.** Terms are the GDPR/OTel standard vocabulary, shared by
  code, API, and legal (D10).

## Consequences

Build slices (**core** = both builds; **managed** = server-local only):

1. `ContentCapture` + `CaptureDecision` in `awaken-runtime-contract`; gate GenAI
   span content attributes on `level` (default `Structured`). *(core, both builds)*
2. `ContentRedactor` port + `Noop` + `RegexPiiRedactor` in
   `awaken-observability`; fold `redact_arguments` in as a source. *(core)*
3. `DataSubjectId` on the neutral request + `DataSubjectResolver` port with
   `NullResolver`; resolve `CaptureDecision` per run/turn in `awaken-runtime-host`;
   thread the opaque id through inference attribution. *(core)*
4. `DataSubjectErasure` + per-record `data_subject_id`/`purpose`/`retention`;
   TTL sweep; downgrade `AWAKEN_TRACE_FILE` to `Structured`-only. *(core)*
5. `control/awaken-data-subject-application`: `DataSubject` aggregate,
   `ConsentGrant`, User Profile application commands, `DataSubjectRepo` port, and
   `RepoDataSubjectResolver`; `stores/awaken-data-subject-store` supplies durable
   SQLite/PostgreSQL adapters and a test-only in-memory adapter. *(managed)*
6. **Fix ①:** keep one `awaken-protocol-managed` public API adapter, enforce the
   separate `user-profiles-2026-03-24` beta, and project the application aggregate;
   delete the former protocol-owned in-memory profile state. *(managed)*
7. `telemetry` ceiling block on Org/Workspace/Agent config + resolver `meet`. *(managed)*
8. `content_capture` request field + decision projection on sessions. *(managed)*
9. Enrollment flow (G3): signed-token URL → server-rendered consent page →
   `ConsentGrant` write; `RepoDataSubjectResolver` gates on it; withdrawal → erasure. *(managed)*
10. `POST /v1/user_profiles/:id/erasure` fans out through stable domain targets;
    Coordinator currently erases captured telemetry and portable ACP sessions,
    while each adapter fences late writes and replays its durable receipt;
    trace/eval/memory adapters join their owning target when subject-keyed storage is implemented;
    optional Art. 15 access. *(managed)*
    Hosted Cloud invokes this same application command through the existing
    service-authenticated private Control boundary. For organization lifecycle,
    `RepoOrganizationPrivacyResolver` inventories the exact Org in the same
    `DataSubjectRepo`, then delegates every erase to that resolver; export reads
    the same aggregates and rejects a cross-Org subject selector. The private
    organization erase/export routes reuse `HttpControlServiceClient` and its
    rotating service credential. This is a process-manager and transport
    projection only: it does not introduce a second resolver, erasure job,
    checkpoint, public Privacy namespace, license check, or entitlement model.
11. Apply the same `CaptureDecision` gate + `data_subject_id` attribution when
    `awaken-eval` records real runs (purpose `eval_recording`, separate consent).
    *(blocked on the `awaken-eval` crate — goal-gap P0)*

## Rejected alternatives

- **A single global `AWAKEN_CAPTURE=on/off` bool.** Cannot express per-tenant
  ceilings, per-subject consent, or the structure/content split — the very axes
  GDPR forces apart.
- **A dedicated `ConsentRecord` aggregate / `/v1/privacy/*` namespace.**
  Duplicates the subject already modelled by `user_profile.trust_grants` and the
  config chain; adds an anemic aggregate for nothing.
- **Redaction as a fourth capture level.** Conflates *which fields* with *how
  scrubbed*; kept orthogonal (level × redactor) to avoid an enum combinatorial
  explosion.
- **Resolving capture inside the runtime.** Would leak scope/consent into the
  execution kernel, violating ADR-0043's runtime-unaware seam.
