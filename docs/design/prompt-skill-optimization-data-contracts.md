# Prompt And Skill Optimization Data Contracts

This document is the implementation contract for
[ADR-0068](../adr/0068-unified-prompt-and-skill-optimization.md). The global
system optimizes a frozen target against a frozen dataset and promotes a winner
through the target's existing owner. This document focuses on the static data,
ports, persistence boundaries, public commands, and GDPR lineage needed by that
flow. Lifecycle transitions are owned by the companion
[state-machine document](prompt-skill-optimization-state-machine.md).

Names below are target contracts. They must be introduced by extending existing
types and stores where noted; equivalent parallel types are forbidden.

## Static Structure

```text
Optimization bounded context
  OptimizationJob (aggregate root; metadata only)
    |- OptimizationTarget + TargetBasePin
    |- DatasetSnapshotRef
    |- OptimizationPolicy + EvaluationPlan
    |- Candidate records + metric/report refs
    |- JobState + revision + lease/cancel facts
    `- PromotionReceiptRef?

  OptimizationJobStore             (new metadata repository)
  OptimizationArtifactStore        (new governed content repository)
    `- implements existing ContentEraser

Existing owners reached through ports
  awaken-eval                      dataset validation, runners, scorers
  RunExecutor                      real execution
  SandboxProvider                  environment creation/adoption/teardown
  ToolExecutor / tool relay        tool calls
  AgentConfig store/publication    Agent prompt promotion
  SkillStore                       Skill winner promotion
  AgentGrader                      Outcome Judge execution
  DataSubjectResolver              purpose/lawful-basis ceiling and erasure
  credential/material resolver     exact secret custody; never stored here
```

### Ownership and dependency rules

| Data or behavior | Authority | Optimization may persist |
|---|---|---|
| Agent instructions | `AgentConfig` revision and publication | base pin, candidate patch, receipt reference |
| Outcome decision | `awaken-ext-goal` and committed Judge Run | pinned Judge snapshot, observation/report reference |
| Skill content | `SkillStore` immutable versions | base hash and non-visible candidate bundle reference |
| Run/tool result | Runtime committed facts and normal ports | derived observation reference |
| Sandbox | `SandboxProvider` | durable `SandboxHandle` while active, teardown receipt |
| credential bytes | credential/material resolver | exact secret-free access/reference only |
| consent/lawful basis/erasure | data-subject aggregate and resolver | resolved processing decision and receipt reference |

An optimization table must never contain a second current Agent config, latest
Skill pointer, Outcome status, Run terminal status, credential material, or raw
personal content.

## Core Identifiers And Pins

Identifiers are opaque strings with stable prefixes on public wires:

```rust
OptimizationJobId("opt_...")
CandidateId("cand_...")
DatasetSnapshotId("ods_...")
DatasetItemId("odi_...")
OptimizationArtifactId("oart_...")
EvaluationRunId("oeval_...")
PromotionReceiptId("oprom_...")
ProcessorReceiptId("oproc_...")
```

Every object includes `workspace_id`; the job additionally records the
controller `org_id` needed for GDPR accountability. Public authorization happens
before repository calls; repositories still enforce intrinsic Workspace
ownership and never store principal/role/PDP decisions.

```rust
enum OptimizationTarget {
    AgentInstructions {
        agent_id: String,
        base_revision: u64,
        base_publication_fingerprint: String,
    },
    OutcomeJudgeInstructions {
        judge_agent_id: String,
        base_snapshot_id: String,
        base_snapshot_fingerprint: String,
    },
    SkillBundle {
        skill_id: SkillId,
        base_version_id: SkillVersionId,
        base_version: u64,
        base_bundle_sha256: String,
    },
}
```

All pins are checked at job preparation and again at promotion. Target kinds are
closed; free-form target table names or patch destinations are invalid.

## Optimization Job Aggregate

```rust
struct OptimizationJob {
    id: OptimizationJobId,
    org_id: String,
    workspace_id: String,
    target: OptimizationTarget,
    dataset_snapshot_id: DatasetSnapshotId,
    policy: OptimizationPolicy,
    evaluation_plan: EvaluationPlan,
    processing: ProcessingEnvelope,
    baseline_report: Option<OptimizationArtifactId>,
    selected_candidate: Option<CandidateId>,
    state: JobState,
    revision: u64,
    cancel_requested_at: Option<Timestamp>,
    lease: Option<JobLease>,
    created_at: Timestamp,
    updated_at: Timestamp,
    terminal_at: Option<Timestamp>,
}
```

`revision` is the optimistic-concurrency fence for every command. `JobLease`
contains worker id, epoch, expiry, and heartbeat; it is an execution claim, not
job state. Target/dataset/policy/processing fields are immutable after
`Created -> Preparing`. A change creates a new job.

```rust
struct OptimizationPolicy {
    max_rounds: u16,
    max_candidates_per_round: u16,
    max_provider_calls: u64,
    max_input_tokens: u64,
    max_output_tokens: u64,
    max_cost_minor_units: u64,
    max_wall_time_seconds: u64,
    proposer: InferencePin,
    semantic_judge: Option<InferencePin>,
    promotion: PromotionPolicy,
}

struct InferencePin {
    executable_snapshot_id: String,
    snapshot_fingerprint: String,
    provider_identity_ref: String,
    model_ref: String,
    backend_ref: String,
    inference_options_sha256: String,
    credential_access_ref: String, // exact, secret-free reference
}
```

The optimizer cannot change model, reasoning, provider, tools, or environment
mid-candidate. A rerun with different pins is a different evaluation run and is
not pooled into the comparison.

## Dataset Contracts

```rust
struct DatasetSnapshot {
    id: DatasetSnapshotId,
    schema_version: u32,
    name: String,
    source: DatasetSource,
    source_revision: String,
    source_sha256: String,
    redaction_policy_version: String,
    scorer_bundle_sha256: String,
    items: Vec<DatasetItemManifest>,
    split_manifest_sha256: String,
    state: DatasetSnapshotState,
    processing: ProcessingEnvelope,
    created_at: Timestamp,
}

struct DatasetItemManifest {
    id: DatasetItemId,
    content_ref: OptimizationArtifactId,
    content_sha256: String,
    split: DatasetSplit,
    oracle: OracleKind,
    dimensions: BTreeSet<String>,
    required_tools: Vec<ToolExpectation>,
    subject_lineage: SubjectLineage,
}

enum DatasetSplit { Train, Validation, Test }
enum OracleKind { Deterministic, HumanGold, IndependentlyAdjudicated,
                  WeakOracle, SelfJudged, PublicReference }
enum DatasetSnapshotState { Active, Invalidated }
```

The proposal worker receives a capability scoped to Train content. The
validation runner may read Validation content; Test content and verdicts are
released only to the sealed-test runner after candidate selection. API list/get
responses return manifest metadata, never hidden content or verdicts.

Runtime-derived items name committed run/thread/snapshot facts in
`DatasetSource`. Synthetic items name the committed fixture revision. Imported
goal-worktree rows additionally store the old file checksum, importer version,
field mapping, dropped fields, and weak-oracle label. Import rejects provider
secrets, absolute home paths, raw environment dumps, missing provenance, and
unassigned splits.

## Evaluation Plan And Tool Contracts

```rust
enum EvaluationMode {
    DeterministicReplay,
    SandboxedOffline,
    EphemeralIntegration,
    LiveCanary,
}

struct EvaluationStage {
    split: DatasetSplit,
    mode: EvaluationMode,
    repetitions: u16,
    runner_version: String,
    scorer_versions: BTreeMap<String, String>,
    gates: Vec<MetricGate>,
    sandbox: Option<SandboxProfile>,
    tools: ToolProfile,
}

struct MetricGate {
    metric: String,
    comparison: GateComparison,
    threshold: Decimal,
    hard: bool,
    minimum_sample_size: u64,
    confidence: Option<ConfidenceRequirement>,
}

enum ExternalToolPolicy {
    Recorded { fixture_set_sha256: String },
    EphemeralIntegration { integration_id: String, cleanup_sla_seconds: u64 },
    LiveCanary { integration_id: String, effect_budget: EffectBudget,
                 reconciliation_policy: String },
}
```

`ToolProfile` freezes visible descriptors, permission policy hash, executor
route, external policy, expected calls/effects, and fixture hashes. A Skill's
`allowed_tools` can only narrow the already-authorized set. Candidate edits that
widen tools, permissions, external hosts, credential usage, or effect budget are
hard failures unless a separately authorized job target explicitly allows that
dimension; the first slices do not.

`SandboxProfile` compiles to the existing `SandboxSpec`. Managed profiles use
Container; trusted local/offline profiles may use Namespace. The admitted
`SandboxCapabilities` snapshot and provider kind are recorded on every
observation. No new sandbox vocabulary is introduced.

## Candidate And Observation Contracts

```rust
struct OptimizationCandidate {
    id: CandidateId,
    job_id: OptimizationJobId,
    parent: CandidateParent,       // baseline or prior candidate
    round: u16,
    ordinal: u16,
    proposer_run_ref: String,
    patch_ref: OptimizationArtifactId,
    materialized_ref: OptimizationArtifactId,
    materialized_sha256: String,
    subject_lineage: SubjectLineage,
    state: CandidateState,
    static_report_ref: Option<OptimizationArtifactId>,
    validation_report_ref: Option<OptimizationArtifactId>,
    test_report_ref: Option<OptimizationArtifactId>,
    rejection: Option<CandidateRejection>,
    created_at: Timestamp,
}

struct EvaluationObservation {
    id: EvaluationRunId,
    job_id: OptimizationJobId,
    candidate_id: Option<CandidateId>, // None means baseline
    dataset_item_id: DatasetItemId,
    split: DatasetSplit,
    repetition: u16,
    frozen_inputs_sha256: String,
    runtime_run_ref: Option<String>,
    sandbox_handle_ref: Option<String>,
    output_ref: Option<OptimizationArtifactId>,
    deterministic_metrics: BTreeMap<String, MetricValue>,
    semantic_judgments_ref: Option<OptimizationArtifactId>,
    usage: UsageMeasurement,
    effect_receipts: Vec<EffectReceipt>,
    failure: Option<EvaluationFailure>,
    subject_lineage: SubjectLineage,
}
```

Observations are append-only and idempotent on
`(job, candidate-or-baseline, item, split, repetition, frozen_inputs_sha256)`.
An infrastructure error is not a zero model score. A report contains separate
denominators for observed cases, valid outputs, provider failures, runtime
failures, and quality failures.

Candidate patches use target-specific closed representations:

- Agent/Judge: complete replacement instruction bytes plus old/new SHA-256;
- Skill: a binary-safe complete candidate bundle plus a path-level diff for
  review. Paths are normalized and revalidated by the Skill application service.

Arbitrary database patches, executable promotion scripts, or target-specific
code hidden in a rubric are invalid.

## GDPR Processing And Artifact Contracts

ADR-0050's existing types remain authoritative. Implementation adds these
`Purpose` variants through the same migrations and exhaustive matches:

```rust
Purpose::PromptSkillOptimization
Purpose::ExternalCanaryValidation
```

The current data-subject aggregate calls every per-purpose record a
`ConsentGrant` even when `basis` is contract or legitimate interest, and its
`LawfulBasis` omits three Article 6 bases. Online personal-content optimization
must first perform one canonical migration inside that aggregate:

```rust
enum LawfulBasis {
    Consent,
    Contract,
    LegalObligation,
    VitalInterests,
    PublicTask,
    LegitimateInterest,
}

enum ProcessingAuthorityStatus {
    Active,
    Pending,
    WithdrawnConsent,
    Objected,
    Expired,
    Revoked,
}

struct ProcessingAuthorityRecord {
    purpose: Purpose,
    basis: LawfulBasis,
    status: ProcessingAuthorityStatus,
    notice_version: String,
    recorded_at: Timestamp,
    assessment_ref: String,
    article_9_condition_ref: Option<String>,
}
```

Old `ConsentGrant` rows are imported once; the canonical store and resolver then
read/write only `ProcessingAuthorityRecord`. There is no compatibility dual
write or second consent repository. Consent requires proof of a freely given,
specific, informed, unambiguous choice and supports withdrawal. Legitimate
interest requires a balancing assessment and supports objection. Legal
obligation/public task records the governing-law/mandate reference. Software
checks record presence, scope, status, and expiry; the controller remains
responsible for the legal determination.

Special-category data is denied by default. A non-empty Article 9 condition
reference, policy approval, and DPIA where required are additional gates; an
Article 6 basis alone is insufficient.

```rust
struct ProcessingEnvelope {
    purpose: Purpose,
    lawful_basis: LawfulBasis,
    processing_authority_record_ref: String,
    article_9_condition_ref: Option<String>,
    capture: ContentCapture,
    subject_lineage: SubjectLineage,
    retention_until: Timestamp,
    region: String,
    restricted: bool,
    dpia_ref: Option<String>,
}

struct SubjectLineage {
    primary: Option<DataSubjectId>,
    additional: BTreeSet<DataSubjectId>,
    derivation: LineageDerivation,
}

struct OptimizationArtifact {
    id: OptimizationArtifactId,
    workspace_id: String,
    media_type: String,
    ciphertext_ref: String,
    plaintext_sha256: String,
    size_bytes: u64,
    classification: DataClassification,
    processing: ProcessingEnvelope,
    created_at: Timestamp,
    deleted_at: Option<Timestamp>,
}
```

`DataClassification` distinguishes non-personal, personal, special-category,
and demonstrated-anonymous content. Pseudonymized is a property of personal
content, not an anonymous class. A candidate inherits the union of source
lineage; only a reviewed anonymization transform can remove it.

`OptimizationArtifactStore` must:

- authorize through trusted Workspace scope and enforce restriction/expiry on
  every read;
- encrypt content with an erasure-capable DEK strategy and keep keys outside job
  rows;
- maintain a subject-to-artifact association for every lineage member;
- implement the existing `ContentEraser` and register with the one
  `DataSubjectResolver` fan-out;
- delete caches, search indexes, derived previews, and encryption keys with the
  primary artifact;
- return an error on partial deletion and remain idempotent;
- never globally deduplicate raw personal bytes across subjects.

The current plain JSON fixture/observation store in `awaken-eval` is allowed for
committed synthetic/non-personal fixtures and local ephemeral output only. It is
not an online personal-content store. A GDPR production composition must reject
`NullResolver`, `NoopRedactor` for admitted Full content, and unbounded
retention.

The same governed indexes must support subject access/export, rectification
lineage, processing restriction, and objection workflows; erasure is not the
only data-subject right. Optimization reports exposed to operators must use
least-privilege views and must not reveal hidden test content or another
subject's data.

### External processor contract

```rust
struct ExternalProcessorBinding {
    id: String,
    processor_name: String,
    purpose: Purpose,
    regions: BTreeSet<String>,
    subprocessors_revision: String,
    dpa_revision: String,
    transfer_mechanism: Option<TransferMechanism>,
    provider_retention_seconds: u64,
    provider_training_use: ProviderTrainingUse,
    deletion_sla_seconds: u64,
    supports_subject_deletion: bool,
}
```

The coordinator resolves this binding before sending content. Region/transfer,
retention, training-use, purpose, and deletion support must satisfy policy. The
receipt records only provider request/correlation ids and deletion status, never
credentials. An external provider that cannot meet deletion/retention policy is
inadmissible for personal data.

## Promotion Contracts

```rust
struct PromoteOptimization {
    job_id: OptimizationJobId,
    candidate_id: CandidateId,
    expected_job_revision: u64,
    expected_target_pin: TargetBasePin,
    approval_ref: Option<String>,
    idempotency_key: String,
}

struct PromotionReceipt {
    id: PromotionReceiptId,
    job_id: OptimizationJobId,
    candidate_id: CandidateId,
    candidate_sha256: String,
    old_target_pin: TargetBasePin,
    new_target_pin: TargetBasePin,
    target_commit_id: String,
    promoted_at: Timestamp,
}
```

Target adapters are intentionally narrow:

- Agent: read exact base revision, apply instruction replacement, call existing
  `put_config_if_revision_scoped`, then normal publication;
- Skill: validate complete bundle, assign exactly the next non-reused ordinal,
  call `SkillStore::append_version` once;
- Outcome Judge: publish a normal tool-free Agent config/snapshot and update the
  composition's pinned Judge reference. It never writes an Outcome decision.

Idempotency is checked by `(target identity, candidate_sha256, idempotency_key)`.
On recovery, an existing matching target commit completes the receipt; a
different current target yields `stale_base`.

## Public API Contract

The initial HTTP surface is Workspace-scoped:

| Method | Path | Semantics |
|---|---|---|
| `POST` | `/v1/optimizations` | create one frozen job; requires idempotency key |
| `GET` | `/v1/optimizations/{id}` | metadata/state only |
| `GET` | `/v1/optimizations/{id}/candidates` | candidate metadata and report summaries |
| `GET` | `/v1/optimizations/{id}/report` | baseline/winner comparison, gate evidence |
| `POST` | `/v1/optimizations/{id}/cancel` | durable cancel intent with expected revision |
| `POST` | `/v1/optimizations/{id}/promote` | explicit CAS promotion |

Create accepts target pin, dataset snapshot id, policy, evaluation plan, purpose,
lawful-basis record, finite retention, and optional DPIA reference. It does not
accept inline datasets, raw credentials, arbitrary shell commands, or hidden
external endpoints. Content upload/import is a separate governed dataset API so
authorization, redaction, lineage, and retention run before job creation.

Errors are stable and closed: `invalid_contract`, `stale_revision`,
`stale_base`, `dataset_invalidated`, `privacy_denied`, `sandbox_unavailable`,
`egress_guarantee_unavailable`, `budget_exhausted`, `not_eligible`, and
`erasure_incomplete`. Adapters map these to public protocol errors; repositories
do not store HTTP status.

## Persistence And Consistency

`OptimizationJobStore` persists aggregate metadata, candidates, append-only
observations, transition events, leases, cancel intent, and receipts. SQLite and
Postgres adapters must pass one conformance suite. Required transactions are:

1. create job plus frozen immutable references;
2. claim/renew/release a lease with monotonic epoch;
3. transition state plus event using expected revision and current lease epoch;
4. record an idempotent observation plus usage/effect receipts;
5. select candidate plus seal test authorization;
6. record target commit plus promotion receipt reconciliation marker;
7. restrict all subject-related jobs/artifacts before asynchronous erasure.

Artifact bytes are outside the metadata transaction. A durable pending reference
is written before upload; the reference becomes readable only after checksum,
encryption, lineage, and retention metadata commit. Orphaned pending objects are
reclaimed. No state transition may point at a readable report/artifact that has
not committed.

Effective retention is the earliest of Org, Workspace, job, subject restriction,
dataset source, and external processor limits. Expiry first denies reads and new
processing, then performs idempotent deletion. Accountability receipts retain no
raw content or reversible subject identifier beyond the separately governed
subject record.

## Contract Invariants

1. One target, one base pin, one immutable dataset snapshot per job.
2. Candidate proposer can read Train only; sealed Test is used once after
   selection.
3. Evaluation uses production ports; reports never rewrite Runtime truth.
4. Deterministic hard gates dominate semantic aggregate scores.
5. Provider/infrastructure failure is not model failure.
6. Candidates are artifacts; only a winner enters Agent or Skill authority.
7. Promotion requires job CAS and unchanged target base.
8. Personal content is referenced, encrypted, finite-retention, multi-subject
   attributable, restrictable, and erasable across every derivative.
9. Credential material, full environment dumps, and bypassable proxy claims are
   never persisted.
10. Unsupported sandbox/egress/privacy guarantees fail before execution.

## Acceptance Evidence

The contracts are implementation-ready when tests prove:

- duplicate dataset ids, missing lineage, unbounded GDPR retention, invalid
  Purpose, and stale pins are rejected;
- train/validation/test capabilities cannot cross-read;
- scorer replay is byte-stable for one frozen observation set;
- a real offline case uses `RunExecutor`, `SandboxProvider`, and the expected
  `ToolExecutor` route;
- current providers reject authenticated external tools without the two egress
  capabilities;
- Agent and Skill promotion CAS is idempotent and stale-base safe;
- erasure restricts immediately, cancels affected work, deletes every registered
  artifact derivative, and reports partial failure honestly;
- SQLite/Postgres stores pass the same lifecycle/concurrency suite.

## Regulatory References

These contracts implement technical controls; they do not make the controller's
legal determination. The governing baseline used by this design is the European
Commission guidance on
[processing principles](https://commission.europa.eu/law/law-topic/data-protection/information-business-and-organisations/principles-gdpr_en),
[lawful grounds](https://commission.europa.eu/law/law-topic/data-protection/information-business-and-organisations/legal-grounds-processing-data_en),
[processor obligations](https://commission.europa.eu/law/law-topic/data-protection/information-business-and-organisations/obligations/controllerprocessor/can-someone-else-process-data-my-organisations-behalf_en),
[international transfers](https://commission.europa.eu/law/law-topic/data-protection/international-dimension-data-protection/rules-international-data-transfers_en),
[DPIAs](https://commission.europa.eu/law/law-topic/data-protection/rules-business-and-organisations/obligations/when-data-protection-impact-assessment-dpia-required_en),
and
[erasure exceptions](https://commission.europa.eu/law/law-topic/data-protection/information-business-and-organisations/dealing-requests-individuals/do-we-always-have-delete-personal-data-if-person-asks_en),
plus the EDPB's
[Opinion 28/2024 on AI models](https://www.edpb.europa.eu/documents/opinion-of-the-board-art-64/opinion-282024-on-certain-data-protection-aspects-related-to_en).
