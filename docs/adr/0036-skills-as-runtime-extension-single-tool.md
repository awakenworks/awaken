# ADR-0036: Skills Are a Runtime Extension — Two Semantic Tools over Optional Files

- Status: Accepted
- Amendment: 2026-08-23 capability-selected delivery section below
- Date: 2026-07-02
- Supersedes: [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) **D4** only
- Retains: ADR-0035 D1–D3, D5–D8 (the provisioning seam still materializes the
  per-run substrate and the trust roots; only the *skill shape* changes)
- Relates to: `design/resources-memory-files-skills.md`,
  `design/tool-and-capability.md`, [ADR-0004](0004-plugin-factory-contributions-and-capability-bound.md)

## 2026-08-23 amendment: one Skill authority, two mutually-exclusive deliveries

This section is authoritative wherever the original D1-D3, D6-D8, consequences,
or implementation text below implies that every Session always exposes the two
semantic Skill tools. The original decision remains the semantic-tool design;
this amendment adds the Anthropic Managed Agents filesystem projection without
adding another Skill catalog or instruction truth.

The effective, frozen Agent/Session toolset selects delivery once per Session:

| Effective capability | Delivery | Model-visible discovery | Full instructions |
|---|---|---|---|
| any of `bash/read/write/edit/glob/grep` enabled | `ManagedFilesystem` | prompt metadata: name, description, exact `SKILL.md` path | model reads `SKILL.md` with ordinary file tools |
| every filesystem tool disabled | `SemanticTools` | `list_skills` | `Skill` returns the selected body |

Both rows consume the same frozen Skill versions and the same `SkillRegistry`.
They are projections, not synchronized implementations:

- `ManagedFilesystem` materializes every selected delivered bundle, including a
  `SKILL.md`-only bundle, scans agent-created Skills at
  `.claude/skills/<name>/SKILL.md` exactly one directory below the root, and does
  **not** expose `list_skills` or `Skill`. The prompt contains metadata and path,
  never the full body.
- `SemanticTools` materializes no delivered Skill tree. Native Runtime exposes
  the two existing tools through the Session dynamic-tool plugin; ACP exports
  the exact same descriptors/executors through the Session MCP server. There is
  no ACP-only eager body injection.
- A selected filesystem/fork Skill with every filesystem tool disabled is an
  invalid Session and fails before inference. It cannot silently receive a path
  that its Agent cannot read.
- A Session cannot switch delivery after its first runtime projection. Recovery
  and rebuild reuse the same value, preventing simultaneous file and tool paths.

Static ownership remains: `SkillStore` owns immutable versions,
`ResolvedSessionResources` owns the frozen selection, `SkillRegistry` owns
discovery/body resolution, and `SessionRuntimeSlot` owns only the derived
delivery projection. Dynamically, Session construction selects the mode,
materializes files **or** wires tools, injects the matching discovery metadata,
then Native inference or ACP MCP calls the selected projection. Any descriptor /
executor mismatch, missing filesystem capability, or attempted mode change
fails closed before the model receives a competing surface.

ADR-0063 D9 remains authoritative for Managed Session version pinning. The
historical unpinned-catalog statements in D7/D8 below apply only to direct,
agent-created run-scoped Skills.

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

### D1: The skill-specific tool surface is exactly two tools

Skills are fronted by two *semantic* tools, and no more:

- **`Skill { skill, args? }`** — activate a skill: return its instructions as a
  tool result. The single activation entry point.
- **`list_skills { category?, query? }`** — discovery (tier 1): return the
  catalog as data (id, name, description, when-to-use, provenance). Token-cheap;
  the model reads it to choose what to activate.

A per-skill tool must never exist. Everything else a skill needs is done with the
**built-in tools** operating on the skill's materialized files (D6): read a
reference with `read`, run a bundled script with `bash`, author/edit a skill with
`write` / `edit`. There is deliberately **no** `load_skill_resource`,
`skill_script`, or `skill_manage` tool — those would duplicate built-ins.

### D2: Discovery is `list_skills` (pull), not a pinned descriptor

The catalog is served by `list_skills` at call time, **not** rendered into the
`Skill` tool descriptor. The `Skill` descriptor is a stable schema plus a "use
`list_skills` to discover" hint, so a changing skill set never perturbs a hashed
descriptor (D7). Tier 1 is metadata only; tier 2 (full instructions) loads on
`Skill` activation; tier 3 (references/scripts) is reached with built-in
`read` / `bash`.

### D3: Activation is an ordinary tool result

`Skill { skill, args? }` returns the skill's instructions as the tool result, the
runtime injects it into the transcript like any tool result and commits it as
truth. The kernel never learns the concept "skill". `disable-model-invocation` is
enforced at the tool; an unknown skill is a model-visible error, not a run abort.

### D4: The extension lives outside the kernel; the kernel is unchanged

`awaken-ext-skills` owns `SkillSpec`, the `SKILL.md` reader, the `SkillRegistry`,
and the two tools. It composes through existing neutral seams — the tool registry
and the permission gate — with no new `awaken-runtime-contract` type. The kernel's
view stays "tools + committed tool results".

### D5: `allowed_tools` is a selection over already-granted tools

A skill's `allowed_tools` narrows what the model may call while the skill is
active; it is enforced at the permission gate (G9/G21), never a grant. Authored
now in `SkillSpec`; enforcement is a later slice.

### D6: Skill environment requirements are explicit and minimal

Every Skill declares one of two execution substrates:

- **`instruction_only` (default)** — activation only expands instructions into
  context. It has no `${SKILL_DIR}`, supporting files, scripts, filesystem tools,
  or Hand requirement. It does not itself require a Sandbox, although another
  Session capability may. MCP prompts always use this form.
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

`list_skills` scans both and tags each entry's **provenance by its root**
(`delivered` vs `agent-created`) — no authoring tool needed to record it.
`${SKILL_DIR}` / `${SESSION_ID}` template tokens in a filesystem Skill resolve to the
materialized path so instructions can point at their own references/scripts, run
via built-in `read` / `bash`. Because materialization is a provisioning-time
export of *data*, this is placement-agnostic: a remote/container sandbox works
the same (the kernel never reads a filesystem).

### D7: Skills are not pinned; replay is by the fact log

The pinned, fingerprinted surface is the **executable configuration** (the
`Skill` / `list_skills` tool schemas, model binding, instructions) — never the
skill catalog or bodies. A skill activation is a committed tool-result fact
([ADR-0006]); replay re-reads that fact, it does not re-resolve the catalog.
`ResumeTicket.catalog_fingerprint` validates only the executable config, which is
skill-set-independent, so a run resumes even if the skill set changed. Keeping the
catalog out of the `Skill` descriptor (D2) is what makes this hold.

[ADR-0006]: 0006-fact-authority-run-record-is-cache.md

### D8: Agent-authored skills are run-scoped; publishing is a separate gate

An agent authoring a skill this run (writing under the workspace root with
`write` / `edit`) makes it usable **this run**: `list_skills` surfaces it
(provenance `agent-created`) and `Skill` can activate it. This is perception, not
authorization — every tool the skill then invokes is still gated per-call. A
run-scoped skill is **not** pinned and does **not** cross the `PromotionGate`
(ADR-0035 D6) into the shared/trusted store; publishing stays an external,
control-plane concern. The "read the skill before rewriting it" guard is provided
by `awaken-ext-state-machine` (a declarative `read-before-write` machine keyed by
path), not a skill-specific tool.

## Consequences

- The skill-specific tool face is fixed at two tools regardless of skill count;
  everything else reuses built-ins on materialized files.
- The pin hazard is removed: skills are runtime data; the fingerprint covers only
  executable config; resume survives a changed skill set.
- Provenance falls out of the two-root layout; agent self-authoring is first-class
  yet cannot self-promote or self-authorize.
- Migration from ADR-0035 D4 is a clean removal (per-skill-tool path, `SkillMount`,
  the sandbox `SkillTool`) plus `awaken-ext-skills`.

## Implementation slices

**Landed** (all in `awaken-ext-skills`, wired by `awaken-coordinator-local`; kernel
unchanged; every commit tested):

- **Two-tool surface** — `Skill` + `list_skills`, catalog out of the descriptor,
  `SkillSpec.provenance`, in-process registry.
- **`$ARGUMENTS`/`$1`..`$9`** argument substitution and **`${SKILL_DIR}`/
  `${SESSION_ID}`** template substitution on activation.
- **Metadata size limits** (name/description caps + bounded catalog entries).
- **Richer `SKILL.md` frontmatter** (`user-invocable`, `argument-hint`,
  `arguments`, `model`, `context`, `agent`, `paths`, `category`, `tags`,
  `version`).
- **Live sandbox discovery** — `Environment::scan_skill_dir` (root stays hidden),
  a `SkillSource` port + `SourceSkillRegistry`/`CompositeSkillRegistry`, delivered
  plus **agent-authored (run-scoped, provenance by root)** skills. Instruction-only
  delivered Skills remain in the host snapshot and are not materialized.
- **tier-3 references/scripts** via built-in `read`/`bash` over materialized files
  (no dedicated tool).
- **Conditional (`paths`) surfacing** — a `RecordingGate` observes touched paths;
  `list_skills` hides a paths-scoped skill until a glob matches.
- **Fork execution** (`context: fork`) via a `SubAgentRunner` port backed by the
  host's `run_subagent`.
- **User `/skill-name`** invocation — the host expands a leading `/name` into the
  skill body (`user_invocable` only).

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

- Reference designs: Claude Code (`Skill` tool + listing attachment); Hermes
  (`skills_list` / `skill_view` / `skill_manage` over a scanned dir).
- [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) (D1–D3,
  D5–D8 retained; D4 superseded here).
