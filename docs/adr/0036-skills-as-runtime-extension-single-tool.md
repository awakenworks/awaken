# ADR-0036: Skills Are a Runtime Extension — Two Semantic Tools over Materialized Files

- Status: Accepted
- Date: 2026-07-02
- Supersedes: [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) **D4** only
- Retains: ADR-0035 D1–D3, D5–D8 (the provisioning seam still materializes the
  per-run substrate and the trust roots; only the *skill shape* changes)
- Relates to: `design/resources-memory-files-skills.md`,
  `design/tool-and-capability.md`, [ADR-0004](0004-plugin-factory-contributions-and-capability-bound.md)

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

### D6: Skills are materialized into the sandbox on two trust roots

Skills live as files in the sandbox (ADR-0035 D6 two roots):

- a **delivered** root — read-only, control-owned, trusted;
- the agent **workspace** — writable, untrusted until promoted.

`list_skills` scans both and tags each entry's **provenance by its root**
(`delivered` vs `agent-created`) — no authoring tool needed to record it.
`${SKILL_DIR}` / `${SESSION_ID}` template tokens in `SKILL.md` resolve to the
materialized path so instructions can point at their own references/scripts, run
via built-in `read` / `bash`. Because materialization is a provisioning-time
export of *data*, this is placement-agnostic: a remote/container sandbox works
the same (the kernel never reads a filesystem).

### D7: Skills are not pinned; replay is by the fact log

The pinned, fingerprinted surface is the **executable configuration** (the
`Skill` / `list_skills` tool schemas, model binding, instructions) — never the
skill catalog or bodies. A skill activation is a committed tool-result fact
([ADR-0006]); replay re-reads that fact, it does not re-resolve the catalog.
`WaitingTicket.catalog_fingerprint` validates only the executable config, which is
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

- **Stage 2 (this ADR's baseline):** `Skill` + `list_skills`, catalog out of the
  descriptor, `SkillSpec.provenance`, in-process registry. No kernel change.
- **Stage 3:** sandbox materialization on the two roots, `${SKILL_DIR}`
  substitution, `list_skills` scanning the sandbox (agent-created skills),
  resources/scripts via built-in `read`/`bash`, authoring via `write`/`edit`.
- **Later:** `read-before-write` via `awaken-ext-state-machine` (needs a
  composable tool-gate seam), `allowed_tools` enforcement, `$1`/`$ARGUMENTS`
  substitution, user `/skill-name` invocation, fork execution.

## References

- Reference designs: Claude Code (`Skill` tool + listing attachment); Hermes
  (`skills_list` / `skill_view` / `skill_manage` over a scanned dir).
- [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) (D1–D3,
  D5–D8 retained; D4 superseded here).
