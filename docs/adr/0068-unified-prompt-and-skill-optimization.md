# ADR-0068: Unified Prompt And Skill Optimization

- Status: Proposed
- Date: 2026-07-27
- Depends on: ADR-0031, ADR-0036, ADR-0041, ADR-0044, ADR-0050,
  ADR-0057, ADR-0063, ADR-0064, ADR-0067
- Detailed contracts:
  [prompt-skill-optimization-data-contracts](../design/prompt-skill-optimization-data-contracts.md)
- Lifecycle:
  [prompt-skill-optimization-state-machine](../design/prompt-skill-optimization-state-machine.md)

## Context

Awaken needs one end-to-end facility that can optimize Agent instructions,
Outcome Judge instructions, and complete Skill bundles. It must support quick
LLM screening and real execution, including sandboxed tools and bounded external
integrations, without allowing an optimizer to mutate production directly.

The pre-design duplication review found these existing authorities:

- `awaken-eval` already owns versioned datasets, provider observations,
  deterministic scorers, optional Judge adapters, and execution through ordinary
  `RunExecutor` ports. The former standalone Admin Assistant Python harness has
  been migrated into this crate as a versioned dataset plus separate live-run and
  offline-score commands; it is not a second evaluation path.
- `awaken-ext-goal` owns Outcome orchestration, its three-state Judge contract,
  production prompt, strict parser, and tool-free `AgentGrader`. Optimization may
  replace a pinned Judge Agent snapshot but cannot add another Grader.
- the config domain owns mutable `AgentConfig` revisions and publication;
  Runtime consumes only immutable `ExecutableAgentSnapshot` values.
- the resource domain's `SkillStore` owns `SkillDefinition`, immutable
  `SkillVersion` bundles, the latest pointer, and bundle hashes. Candidate storage
  must not become a second Skill repository.
- `SandboxProvider`, `Sandbox::spawn`, `prepare_environment`, and
  `SandboxCapabilities` are the only environment-isolation seam. `ToolExecutor`
  and the existing tool relay are the execution seams for local or remote tools.
- ADR-0050's `DataSubjectResolver`, `Purpose`, capture lattice, subject-tagged
  stores, and erasure fan-out are the privacy authority. Optimization must extend
  that system rather than inventing a parallel consent or deletion service.

The old goal worktree contains useful cases, tool expectations, timing/error
measurements, and provider scripts, but its `awaken-eval` implementation is an
older parallel track. Only data may be imported through a versioned, reviewed
importer; none of its orchestration or store code is copied.

The two external SkillOpt examples establish the correct quality boundary. An
LLM can propose edits, but admission, parsers, compilers, CLIs, golden behavior,
runtime tests, and audited held-out outcomes are deterministic or real-world
oracles. LLM-only evaluation is therefore useful as a cheap screen, not as the
promotion authority.

## Decision

### D1: One optimization control flow, with existing domain owners retained

Optimization is a control-plane application workflow. It owns a durable
`OptimizationJob`, candidate lineage, evaluation scheduling, comparison, and a
promotion request. It does not own Agent configuration, Outcome evaluation,
Skill resources, Runtime execution, tools, sandboxes, credentials, or consent.

```text
Optimization API / operator
        |
        v
OptimizationCoordinator ----> OptimizationJobStore
        |                         + governed artifact references
        |
        +--> candidate proposer (LLM adapter; train view only)
        +--> awaken-eval (the only dataset/scorer/evaluation library)
        |       +--> ordinary RunExecutor
        |       +--> SandboxProvider / ToolExecutor / existing relays
        |       `--> deterministic contract and runtime gates
        |
        `--> target-owned promotion adapter
                +--> AgentConfig CAS + publication
                +--> SkillStore::append_version
                `--> pinned Judge Agent publication
```

The optimization coordinator is a new orchestration responsibility, not a new
evaluation engine. Its evaluator port must be implemented by the same
`awaken-eval` runners and scorers used by CLI/offline evaluation. Online APIs may
schedule those runners but may not reconstruct prompts, parsers, tool loops, or
scores in HTTP handlers.

### D2: Three targets, each promoted by its existing authority

An `OptimizationTarget` is exactly one of:

1. `AgentInstructions`: pins the Workspace, Agent id, base config revision, and
   publication fingerprint. A winner changes only the instructions field and is
   committed by config CAS and normal publication.
2. `OutcomeJudgeInstructions`: pins the Judge `ExecutableAgentSnapshot` id and
   fingerprint. A winner is published as a replacement pinned Judge Agent
   snapshot and consumed by the existing `AgentGrader`. The compiled
   `DEFAULT_JUDGE_INSTRUCTIONS` remains a safe fallback; changing that built-in
   constant requires an ordinary source patch and review, never a database-side
   hidden override.
3. `SkillBundle`: pins `SkillId`, base `SkillVersionId`, ordinal, and bundle
   SHA-256. A winner is appended through `SkillStore::append_version`; candidates
   remain optimization artifacts and never appear as visible Skill versions.

Promotion is explicit and CAS-protected. If the live target no longer equals the
base pin, the job is rejected with `stale_base`; it is never silently rebased or
merged.

### D3: Evaluation is a cost ladder, not an LLM vote

Every job freezes dataset membership, target base, proposer and evaluator model
pins, inference controls, tools, environment, scorer versions, and quality gates
before the baseline runs. Train, validation, and sealed test membership use
stable item ids and snapshot hashes.

The ladder is:

1. deterministic static checks: schema, size, forbidden content, bundle/path
   safety, parser/admission/compiler checks;
2. optional tool-free LLM screen for semantic triage;
3. real validation through ordinary Runtime execution in one isolated sandbox
   per case or explicitly safe reuse group;
4. deterministic behavioral gates, with an optional tool-free Judge only for
   semantic dimensions that have no deterministic oracle;
5. one sealed test run for the selected candidate;
6. explicit promotion.

A candidate passes only when every hard gate passes. Aggregate LLM score cannot
offset an unsafe accept, schema violation, secret leak, permission widening,
compile/runtime regression, or required-tool mismatch. Provider and
infrastructure failures are reported separately from model quality.

### D4: Four execution modes share the same evaluation contracts

`EvaluationMode` is a closed set:

- `deterministic_replay`: no provider or live tool; re-score frozen observations;
- `sandboxed_offline`: real Runtime and local/recorded tools, network `None`;
- `ephemeral_integration`: allowlisted external test systems with disposable
  tenant/account/data and no production side effects;
- `live_canary`: narrowly scoped production-like reads or idempotent writes,
  explicit approval, reconciliation, and the strongest privacy/security policy.

Mode changes execution adapters, not dataset or scoring semantics. Results are
never compared as if they came from the same environment unless all frozen pins
match.

### D5: Sandbox and external-tool admission fail closed

Cloud or multi-tenant real evaluation requires `IsolationClass::Container`.
Trusted local/offline evaluation may use `Namespace`. `Workdir` is allowed only
for an explicit trusted-development profile and cannot host an untrusted opaque
CLI or online personal data.

`prepare_environment` must admit every real-run plan. `NetworkPolicy::None` is
the default. Public external tools require a provider-enforced no-bypass
allowlist. Authenticated external tools additionally require
`supports_secret_egress_without_bypass()`: both provider-enforced allowlisting
and egress-only secret substitution. Process proxy variables do not satisfy this
guard.

The built-in Namespace and Container providers do not currently claim both
authenticated-egress capabilities. Consequently the first real slice supports
recorded/local tools with network disabled; authenticated external-tool
optimization remains fail-closed until a provider proves the capability suite.
The existing host MCP relay may inject Worker-held credentials only after that
admission succeeds.

External effects use one of three policies:

- `Recorded`: return frozen responses; no external call;
- `EphemeralIntegration`: use a disposable external environment and destroy it
  after the case;
- `LiveCanary`: attach an idempotency key, effect budget, approval, audit receipt,
  and reconciliation procedure.

### D6: GDPR governance applies before capture, not after optimization

Optimization adds `PromptSkillOptimization` and
`ExternalCanaryValidation` to the existing closed `Purpose` vocabulary. These
are migrations of ADR-0050's aggregate and resolver; no parallel consent table
or privacy service is introduced.

Before online personal-content optimization is enabled, the same data-subject
aggregate must also correct its current consent-shaped lawful-basis model. The
canonical Article 6 basis set must cover consent, contract, legal obligation,
vital interests, public task, and legitimate interests. Consent withdrawal,
legitimate-interest objection, and expiry/revocation of a statutory assessment
are different facts and cannot all be represented as `ConsentStatus::Withdrawn`.
The migration replaces the canonical grant record and imports old rows once;
there are no dual writes or two resolvers. Special-category content is rejected
unless an explicit Article 9 condition/assessment reference and required DPIA
are present.

Every dataset item, observation, candidate, Judge output, tool transcript, and
external processor receipt carries a governed content reference and subject
lineage. Job rows contain metadata and references, not raw prompt/tool content.
Pseudonymized data remains personal data. Full content is admitted only when the
resolved lawful basis, purpose, scope ceiling, request, and subject restriction
permit it; otherwise capture is `Structured` or `Off`.

The GDPR deployment profile requires:

- an explicit finite retention deadline; unbounded `None` retention is rejected;
- encryption in transit and envelope encryption at rest, with erasure-capable
  per-object or subject-scoped keys;
- no global content deduplication for raw personal content where one subject's
  erasure would retain bytes through another reference;
- subject-to-artifact many-to-many lineage, because transcripts and tool results
  can concern multiple people;
- immediate restriction on erasure/objection, followed by fan-out over datasets,
  observations, candidates, Judge outputs, caches/indexes, sandboxes, backups
  according to retention policy, and registered external processors;
- processor records covering purpose, region, retention, training use,
  sub-processors, deletion SLA, and international-transfer mechanism;
- a DPIA/approval gate before large-scale, special-category, systematic
  monitoring, or `live_canary` processing.

An erasure exception or legal hold records a scoped decision and restricts the
data; it must not be reported as completed erasure. A partial fan-out keeps the
subject restricted and returns an error rather than a clean receipt.

### D7: Dataset snapshots are immutable, but invalidatable

The optimizer receives train content only. Validation verdicts and all sealed
test content remain inaccessible to the proposer. Snapshot membership and hashes
are immutable for reproducibility, but a subject erasure or source withdrawal
marks the snapshot `invalidated`; it can no longer start or promote a job. A new
snapshot is built without the affected items. Immutable metadata may remain as
accountability evidence only when it contains no erased content or reversible
identifier.

Candidate artifacts inherit the union of their source subject lineage until a
documented transformation establishes anonymity. Detecting personal or secret
material in a candidate quarantines it; redaction does not automatically make it
anonymous.

The goal-worktree corpus is admitted only by an importer that maps every field,
records source revision/checksum, labels weak or self-judged oracles, applies
redaction/subject lineage, and freezes train/validation/test membership. Its code,
prompts, store, and provider scripts are not runtime dependencies.

### D8: Promotion requires human or policy authority, never optimizer authority

The proposer and evaluator can make a candidate `eligible`; neither can promote.
`POST .../promote` requires the target-specific authorization and an expected
job revision plus expected live target pin. Automatic promotion is allowed only
for an explicitly configured policy whose deterministic hard gates, sample size,
confidence threshold, cost/effect budget, privacy class, and target scope are all
satisfied. `live_canary`, permission/tool widening, new external destinations,
and any candidate with personal-data lineage require explicit approval.

Promotion writes one target-owned revision/version, then stores a promotion
receipt. A crash between those actions is reconciled by the target id plus
candidate hash and never appends a second Skill version or Agent revision.

### D9: Delivery is staged by capability

Required first slice:

1. keep all CLI/offline evaluation in `awaken-eval` and remove old harness
   callers;
2. implement the contracts and durable state machine in the control plane;
3. support `AgentInstructions` and `SkillBundle` with synthetic or governed
   datasets, deterministic replay, and sandboxed offline real runs;
4. promote through AgentConfig CAS/publication and `SkillStore::append_version`;
5. register optimization artifact storage in ADR-0050 erasure fan-out.

Required second slice:

1. configure optimized pinned Outcome Judge snapshots through the existing
   `AgentGrader`;
2. add ephemeral external integrations only after enforced allowlisting is
   proven;
3. add external processor/deletion receipts and DPIA policy gates.

Explicitly deferred:

- authenticated external MCP/tools until no-bypass allowlist plus egress secret
  substitution is implemented and tested;
- `live_canary` until idempotency, reconciliation, authorization, privacy, and
  effect-budget E2E evidence exists;
- automatic mutation of compiled built-in prompts;
- training/fine-tuning model weights; this ADR optimizes prompt/Skill artifacts.

## Consequences

### Positive

- one evaluator and one execution truth serve offline CLI and online jobs;
- LLMs are used where semantic proposal/judgment helps, while deterministic and
  real-runtime evidence remains authoritative;
- candidates cannot bypass config publication, Outcome orchestration, Skill
  versioning, tool execution, sandbox admission, credential custody, or GDPR
  erasure;
- train/validation/test leakage, stale-base promotion, provider drift, and
  personal-data deletion have explicit contracts and terminal outcomes.

### Negative and accepted

- online optimization needs durable job metadata and governed artifact storage;
- real evaluation is slower and more expensive than LLM-only scoring;
- current sandbox providers intentionally block authenticated external-tool
  evaluation until stronger egress capabilities exist;
- erasure can invalidate a reproducible snapshot and force a new experiment.

### Rejected alternatives

- **LLM-only evaluation.** Rejected because it cannot prove parser, compiler,
  tool, sandbox, side-effect, or runtime behavior and may share proposer bias.
- **Copy either external SkillOpt implementation.** Rejected because both are
  repository-specific and would duplicate Awaken's Runtime, eval, SkillStore,
  and sandbox owners. Their datasets and method inform adapters, not authority.
- **Store every candidate as a SkillVersion.** Rejected because it pollutes the
  visible immutable resource history and creates a second meaning for latest.
- **A second optimization-specific sandbox/tool layer.** Rejected; existing
  provisioning and `ToolExecutor` seams already own those responsibilities.
- **Proxy-variable egress controls.** Rejected as bypassable by arbitrary
  workloads.
- **Delete personal fields only from the final dataset.** Rejected because raw
  observations, candidates, caches, sandboxes, and processors would remain.
