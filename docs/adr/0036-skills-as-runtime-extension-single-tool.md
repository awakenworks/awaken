# ADR-0036: Skills Are a Runtime Extension Fronted by a Single Tool

- Status: Accepted
- Date: 2026-07-02
- Supersedes: [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) **D4** only
- Retains: ADR-0035 D1–D3, D5–D8 (the provisioning seam still materializes
  resources/files and the per-run substrate; only the *skill shape* changes)
- Relates to: `design/resources-memory-files-skills.md`,
  `design/tool-and-capability.md`, [ADR-0004](0004-plugin-factory-contributions-and-capability-bound.md)

## Context

ADR-0035 D4 modeled a skill as a per-skill dynamic `RawTool`: each provisioned
skill surfaced to the model as its own tool whose call returned the `SKILL.md`
body. That decision optimized for "one provisioning seam, zero runtime change, no
skills extension," and to hit "no extension" it collapsed a skill into a single
tool.

That collapse is wrong at three points:

- **Tool-face blow-up.** N skills put N tool descriptors into every inference
  request, spending the model's tool budget and degrading tool selection. The
  reference design (Claude Code) and the mature reference implementation both
  front the *entire* skill set with **one** tool and present the set as *data*.
- **No activation model.** A per-skill tool has nowhere to carry a skill's
  `allowed_tools` scoping, `when-to-use`, model override, or `user-invocable` /
  `disable-model-invocation` controls — the tool call is the whole story.
- **A false dichotomy.** "The kernel must not know skills" (correct) was conflated
  with "there must be no skills extension" (unnecessary). An extension that lives
  entirely in plugin/tool space keeps the kernel just as neutral.

## Decision

### D1: One `Skill` tool, never per-skill tools

The whole activatable skill set is fronted by a single tool, id `Skill`
(`awaken-ext-skills::SKILL_TOOL_ID`), with input `{ skill, args? }`. A per-skill
tool must never exist. The model's tool face carries at most one skill entry
regardless of how many skills are offered.

### D2: Discovery is data carried by the tool descriptor

The activatable-skill **catalog** (id + description + when-to-use, budget-capped
per entry) is rendered into the `Skill` tool's descriptor by
`skill_tool_descriptor`. Only model-invocable skills are listed. The model reads
what is available; it does not see a list of callable tools. (A later slice may
move the catalog to a refreshed, hidden context contribution to support dynamic /
conditional listing; Stage 1 keeps it in the descriptor.)

### D3: Activation is an ordinary tool result

Calling `Skill { skill, args? }` returns the skill's instructions as the tool
result, which the runtime injects into the transcript like any tool result and
commits as truth. The kernel never learns the concept "skill" — it sees one tool
and a tool result. `disable-model-invocation` is enforced at the tool
(model activation refused); an unknown skill is a model-visible error, not a run
abort.

### D4: The extension lives outside the kernel; the kernel is unchanged

`awaken-ext-skills` owns `SkillSpec`, the `SKILL.md` reader, the `SkillRegistry`,
the `Skill` tool, and (in later slices) the `allowed_tools` gate and conditional
activation. It composes through existing neutral seams — the tool registry, the
permission gate, and (later) the plugin hooks — with **no** new kernel or
`awaken-runtime-contract` type required for Stage 1. Skills are no longer a
sandbox `Mount`; `awaken-sandbox-local` provisions isolation tools and resources
only.

### D5: `allowed_tools` remains a selection over already-granted tools

A skill's `allowed_tools` narrows what the model may call while the skill is
active; it is enforced at the permission gate (G9/G21), never a grant. This is a
later slice consuming committed activation state; the authoring field is fixed
now so the shape is stable.

## Consequences

- The model's tool face is O(1) in skills, not O(n); the catalog scales as text.
- Skill authoring gains a real home (`SkillSpec` / `SKILL.md` frontmatter) with
  room for when-to-use, allowed-tools, and invocation controls.
- The kernel stays provenance- and skill-agnostic (ADR-0035 D2 preserved): its
  view is one tool plus a committed tool result.
- Migration is a clean removal (the per-skill-tool path, `SkillMount`, the sandbox
  `SkillTool`) plus the new `awaken-ext-skills` crate; resource provisioning is
  untouched.

## Non-Goals (Stage 1)

- Filesystem/MCP-backed skill registries and `paths`-conditional activation.
- `allowed_tools` enforcement and `arguments` substitution (`$1`, `$ARGUMENTS`).
- User `/skill-name` invocation as a distinct front door.
- Fork (sub-agent) execution mode. These are later slices; the `Skill` tool and
  `SkillSpec` are designed to admit them without another supersession.

## References

- Reference design: Claude Code single `Skill` tool + skill listing attachment.
- [ADR-0035](0035-environment-provisioning-tools-skills-resources.md) (D1–D3,
  D5–D8 retained; D4 superseded here).
