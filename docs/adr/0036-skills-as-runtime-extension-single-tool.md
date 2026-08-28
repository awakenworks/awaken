# ADR-0036: Skills Are a Runtime Extension — One Registry, One Projection per Profile

- Status: Accepted
- Amendment: 2026-08-29 Managed filesystem convergence section below
- Date: 2026-07-02
- Supersedes: [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) **D4** only
- Retains: ADR-0035 D1–D3, D5–D8 (the provisioning seam still materializes the
  per-run substrate and the trust roots; only the *skill shape* changes)
- Relates to: `design/resources-memory-files-skills.md`,
  `design/tool-and-capability.md`, [ADR-0004](0004-plugin-factory-contributions-and-capability-bound.md)

## 2026-08-29 amendment: Managed Skills have one filesystem projection

This section is authoritative wherever the original D1-D3, D6-D8, consequences,
or implementation text below implies that a Managed Session may fall back to the
two semantic Skill tools. It replaces the 2026-08-23 capability-only selection:
the execution profile is the first discriminator, and capability is an admission
condition inside that profile.

| Profile and selection | Filesystem capability | Skill projection | Outcome |
|---|---|---|---|
| Managed + frozen selected Skill | enabled | `ManagedFilesystem` | metadata/path prompt; model reads exact materialized `SKILL.md` bytes |
| Managed + realized repository Skill | exact `read` enabled | `ManagedFilesystem` | scan the frozen checkout once at `.claude/skills/<name>/SKILL.md`; disclose metadata/path |
| Managed + realized repository Skill | exact `read` disabled | none | repository remains mounted as code input; no repository Skill projection |
| Managed + frozen selected Skill | disabled | none | fail before inference |
| Managed + no frozen selected Skill | disabled | none | non-Skill content may still use `SemanticTools`; no Skill tools are built |
| direct/non-Managed compatibility caller | enabled | filesystem | existing progressive disclosure |
| direct/non-Managed compatibility caller | disabled | semantic adapter | existing `list_skills`/`Skill` migration surface |

There is one authority and one projection for each profile:

- An attached Managed Skill is selected by `AgentSkillBinding`, resolved to an
  exact `ResolvedSkillBinding`, and loaded as hash-verified `SkillVersion` bytes.
  The runtime materializes those bytes under its read-only `.skills` projection.
- A `github_repository` is already a resolved Session Resource and is realized
  once into the Session Environment before Skill construction. When the exact
  `read` capability is enabled, the runtime scans only that frozen checkout's
  fixed `.claude/skills/<skill-name>/SKILL.md` layout once. It adds the resulting
  neutral files to the same `CompositeSkillRegistry` as attached versions and
  path-qualifies identity, so equal display names coexist. It never performs a
  second clone/fetch, creates a Skill-specific repository/store, watches the
  remote, or rescans the current Session after repository files change. A new
  Session receives a new snapshot from its own realized checkout.
- `plugin_config.skills_dir` remains a direct/non-Managed authored-workspace
  compatibility setting. It cannot change the Managed repository path. An
  unmounted workspace `.claude/skills`, host-static `SkillSpec`, live durable
  catalog/cache, and lazy MCP prompt registry are not Managed authorities. MCP
  prompts-as-skills fail Managed admission; Managed capabilities advertise only
  version-backed attached catalog ids. These sources cannot replace missing or
  corrupt attached binding bytes or enter through repository discovery.
- The runtime builds one registry from these admitted attached and repository
  inputs, and derives prompt metadata and paths from that registry. It never
  constructs `ListSkillsTool`, `SkillTool`, their activation state, or their
  permission gate for the Managed profile.
- Direct/non-Managed callers may temporarily use the two-tool adapter. The
  adapter receives the already-built registry; it owns neither a second catalog
  nor a second body/version store. Removing that adapter later therefore does
  not migrate Managed state.
- The existing Managed API wire lowering is a thin conversion to canonical
  `AgentSkillBinding`. No separate Agent SDK adapter/runtime was identified or
  added by this decision. Any future provider or Agent SDK input adapter may
  only perform that lowering; it must not introduce a registry, executor,
  activation state, or version cache.
- A physical Session freezes its content-delivery choice. Recovery and auxiliary
  snapshots reuse it; a recovered Managed Skill cannot reopen a semantic path.

Static ownership remains: `SkillStore` owns immutable attached versions;
`ResolvedSessionResources` owns the frozen Skill selection and Repository
inputs; Repository realization owns the one checkout; `SkillRegistry` owns
discovery/body resolution; and `SessionRuntimeSlot` owns only the derived content
projection. Dynamically, repository realization succeeds before root selection;
Managed construction verifies attached bytes, applies the exact `read` gate,
snapshots each fixed repository root, builds/materializes one registry, injects
metadata, then ordinary `read`/`bash` tools consume it. Direct construction may
instead wrap its registry in the compatibility adapter. Repository realization
failure, unsafe mount, missing attached bytes, hash mismatch, missing filesystem
capability, unsupported lazy prompt source, or attempted mode change fails
closed before the model receives a competing surface.

ADR-0063 D9 remains authoritative for Managed Session version pinning. The
run-scoped portions of D7/D8 below apply only to direct, agent-created Skills.

## Context

ADR-0035 D4 modeled a skill as a per-skill dynamic `RawTool`: each provisioned
skill surfaced to the model as its own tool whose call returned the `SKILL.md`
body. That collapse is wrong at three points:

- **Tool-face blow-up.** N skills put N tool descriptors into every inference
  request, spending the model's tool budget and degrading tool selection. The
  references (Claude Code, Hermes) front the *entire* skill set with a tiny fixed
  tool surface and present the set as *data*.
- **No activation model.** A per-skill tool has nowhere to carry `allowed_tools`
  scoping, `when-to-use`, model override, or `user-invocable` /
  `disable-model-invocation` controls.
- **A false dichotomy.** "The kernel must not know skills" (correct) was conflated
  with "there must be no skills extension" (unnecessary). An extension living
  entirely in plugin/tool space keeps the kernel just as neutral.

Reference cross-check: Claude Code exposes one `Skill` activation tool plus a
pushed listing attachment, and does resource/script/authoring through generic
file/bash tools. Hermes exposes `skills_list` + `skill_view` + `skill_manage`
over a scanned skills directory. Both keep skills **unpinned** and **dynamic**,
and both let the agent author skills mid-session.

## Decision

### D1: The direct compatibility tool surface is exactly two tools

When a direct/non-Managed caller cannot use filesystem disclosure, Skills are
fronted by two *semantic* compatibility tools, and no more:

This is an Awaken migration contract, not protocol or runtime equivalence with
Anthropic's Agent SDK. Managed compatibility is the filesystem progressive-
disclosure row above; it never routes through these tools.

- **`Skill { skill, args? }`** — activate a skill: return its instructions as a
  tool result. The single activation entry point.
- **`list_skills { category?, query? }`** — discovery (tier 1): return the
  catalog as data (id, name, description, when-to-use, provenance). Token-cheap;
  the model reads it to choose what to activate.

A per-skill tool must never exist. Managed Sessions expose neither semantic tool.
Everything else a skill needs is done with the
**built-in tools** operating on the skill's materialized files (D6): read a
reference with `read`, run a bundled script with `bash`, author/edit a skill with
`write` / `edit`. There is deliberately **no** `load_skill_resource`,
`skill_script`, or `skill_manage` tool — those would duplicate built-ins.

### D2: Direct semantic discovery is `list_skills`, not a pinned descriptor

For the direct semantic adapter, the catalog is served by `list_skills` at call
time, **not** rendered into the
`Skill` tool descriptor. The `Skill` descriptor is a stable schema plus a "use
`list_skills` to discover" hint, so a changing skill set never perturbs a hashed
descriptor (D7). Tier 1 is metadata only; tier 2 (full instructions) loads on
`Skill` activation; tier 3 (references/scripts) is reached with built-in
`read` / `bash`.

### D3: Direct semantic activation is an ordinary tool result

For the compatibility adapter, `Skill { skill, args? }` returns the Skill's
instructions as the tool result, the
runtime injects it into the transcript like any tool result and commits it as
truth. The kernel never learns the concept "skill". `disable-model-invocation` is
enforced at the tool; an unknown skill is a model-visible error, not a run abort.

### D4: The extension lives outside the kernel; the kernel is unchanged

`awaken-ext-skills` owns `SkillSpec`, the `SKILL.md` reader, the `SkillRegistry`,
and the direct compatibility tools. Managed filesystem disclosure and the direct
adapter compose through existing neutral seams — prompts/filesystem or the tool
registry/permission gate — with no new `awaken-runtime-contract` type. The
kernel's view stays ordinary files/tools plus committed facts.

### D5: `allowed_tools` is a selection over already-granted tools

A skill's `allowed_tools` narrows what the model may call while the skill is
active; it is enforced at the permission gate (G9/G21), never a grant. Authored
now in `SkillSpec`; enforcement is a later slice.

### D6: Skill environment requirements are explicit and minimal

Every Skill declares one of two execution substrates:

- **`instruction_only` (default)** — semantic activation only expands
  instructions into context. In the direct compatibility profile it has no
  `${SKILL_DIR}`, supporting files, scripts, filesystem tools, or Hand
  requirement. A Managed selection still requires a Sandbox because its only
  public contract is an on-demand materialized `SKILL.md` path. MCP prompts use
  this form only for direct/non-Managed callers.
- **`filesystem`** — activation may use bundled references/scripts/assets through
  built-in tools. It therefore requires a filesystem-capable Session environment;
  tool execution may use the local executor or a remote Hand.

For uploaded bundles, any file besides `SKILL.md` objectively upgrades the Skill
to `filesystem`, even if its frontmatter claims otherwise. A `SKILL.md`-only bundle
stays instruction-only unless it explicitly declares `environment: filesystem`.
This inference prevents metadata from hiding a real capability requirement.
Likewise, a configured filesystem Skill without a materialized directory is
rejected fail closed instead of being exposed with a broken `${SKILL_DIR}`.

Filesystem Skills live on two trust roots:

- a **delivered** root — read-only, control-owned, trusted;
- the agent **workspace** — writable, untrusted until promoted.

In the direct semantic profile, `list_skills` scans both and tags each entry's
**provenance by its root** (`delivered` vs `agent-created`) — no authoring tool
is needed to record it.
`${SKILL_DIR}` / `${SESSION_ID}` template tokens in a filesystem Skill resolve to the
materialized path so instructions can point at their own references/scripts, run
via built-in `read` / `bash`. Because materialization is a provisioning-time
export of *data*, this is placement-agnostic: a remote/container sandbox works
the same (the kernel never reads a filesystem).

### D7: Managed Skills are pinned; direct run-scoped Skills replay by facts

Managed Sessions pin each attached Skill as a `ResolvedSkillBinding` and verify
its exact `SkillVersion` bundle hash before materialization. Recovery reloads
those bytes; it never consults the current catalog. Repository Skills instead
derive once from the recovered Session's same realized Repository checkout and
fixed path, never from the remote or a Skill catalog. For direct run-scoped or
agent-created Skills, a semantic activation remains a committed tool-result fact
([ADR-0006]); replay re-reads that fact instead of re-resolving a changing
catalog. The direct adapter's executable tool schemas remain skill-set
independent because the catalog stays out of the descriptor (D2).

[ADR-0006]: 0006-fact-authority-run-record-is-cache.md

### D8: Agent-authored skills are run-scoped; publishing is a separate gate

For a direct semantic caller, an agent authoring a Skill this run (writing under
the workspace root with `write` / `edit`) makes it usable **this run**:
`list_skills` surfaces it (provenance `agent-created`) and `Skill` can activate
it. This is perception, not authorization — every tool the Skill then invokes
is still gated per-call. A run-scoped Skill is **not** pinned and does **not**
cross the `PromotionGate`
(ADR-0035 D6) into the shared/trusted store; publishing stays an external,
control-plane concern. The "read the skill before rewriting it" guard is provided
by `awaken-ext-state-machine` (a declarative `read-before-write` machine keyed by
path), not a skill-specific tool.

## Consequences

- Managed Sessions have no Skill-specific tool face; direct compatibility has a
  fixed two-tool adapter regardless of Skill count. Both reuse one registry.
- Managed replay uses frozen attached binding/version/hash bytes plus a one-time
  snapshot of the same realized Repository checkout. Direct run-scoped replay
  remains fact-based and independent of a changed catalog.
- Provenance falls out of the two-root layout; agent self-authoring is first-class
  yet cannot self-promote or self-authorize.
- Migration from ADR-0035 D4 retains no per-Skill tool, `SkillMount`, or sandbox
  `SkillTool`; Managed filesystem disclosure and the direct semantic adapter are
  mutually exclusive projections of `awaken-ext-skills`.

## Implementation slices

**Landed** (`awaken-ext-skills` owns behavior; the Runtime Host owns the single
registry projection; kernel unchanged):

- **Profile-specific surface** — no semantic Skill tools for Managed filesystem
  delivery; direct compatibility retains `Skill` + `list_skills` over the same
  in-process registry, with the catalog out of the descriptor.
- **`$ARGUMENTS`/`$1`..`$9`** argument substitution and **`${SKILL_DIR}`/
  `${SESSION_ID}`** template substitution on activation.
- **Metadata size limits** (name/description caps + bounded catalog entries).
- **Richer `SKILL.md` frontmatter** (`user-invocable`, `argument-hint`,
  `arguments`, `model`, `context`, `agent`, `paths`, `category`, `tags`,
  `version`).
- **Sandbox discovery** — `Environment::scan_skill_dir` (root stays hidden), a
  `SkillSource` port + `SourceSkillRegistry`/`CompositeSkillRegistry`, delivered
  plus **agent-authored (run-scoped, provenance by root)** direct Skills and
  scan-once, path-qualified Managed repository Skills. Instruction-only direct
  delivered Skills remain in the host snapshot and are not materialized.
- **tier-3 references/scripts** via built-in `read`/`bash` over materialized files
  (no dedicated tool).
- **Conditional (`paths`) surfacing** — a `RecordingGate` observes touched paths;
  `list_skills` hides a paths-scoped skill until a glob matches.
- **Fork execution** (`context: fork`) via a `SubAgentRunner` port backed by the
  host's `run_subagent`.
- **User `/skill-name`** invocation — the host expands a leading `/name` from the
  active profile's one registry (`user_invocable` only).

**Remaining:**

- **`allowed_tools` enforcement** — the field is authored and carried; enforcing
  it needs the composable tool-gate to consult active-skill state (the same seam
  `read-before-write` needs).
- **`read-before-write` guard** via `awaken-ext-state-machine`.
- **True read-only delivered root** — provenance is by workspace subdir today;
  the single sandbox root does not yet enforce read-only on the delivered set.
- **Durable resume of run-scoped skill state** — the touched-path / active-skill
  record is in-memory; a durable resume must rebuild it from committed facts.

### D9: Session environments are lazy and first-use synchronized

Session creation freezes the Environment, Resource, Skill, and MCP projections but
does not synchronously create a Hand/Sandbox. The first execution that needs the
Session context acquires the per-Session lifecycle mutex, provisions exactly one
environment, and blocks until it is ready; concurrent callers join the same
critical section and reuse the result.

A future background prewarm may call the same creation path, but it must atomically
persist the resulting opaque environment binding before treating the prewarm as
successful. Starting an untracked background Sandbox is forbidden because a crash
would leave an orphan that recovery cannot adopt.

## References

- [Anthropic Managed Agents: repository Skills](https://platform.claude.com/docs/en/managed-agents/skills#add-skills-from-a-github-repository)
  (fixed repository-root `.claude/skills` scan at Session start; `read` gate;
  repository and attached Skills coexist).
- Reference designs: Claude Code (`Skill` tool + listing attachment); Hermes
  (`skills_list` / `skill_view` / `skill_manage` over a scanned dir).
- [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) (D1–D3,
  D5–D8 retained; D4 superseded here).
