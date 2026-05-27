# ADR-0020: Skill Visibility — Mechanism-Policy Separation

- **Status**: Accepted
- **Date**: 2026-04-02
- **Depends on**: ADR-0015 (Plugin Activation), ADR-0016 (Tool Interception Pipeline)

## Context

The skill subsystem (`awaken-ext-skills`) discovers skills from filesystem, MCP, and
embedded sources and unconditionally injects **all** of them into the LLM catalog
before every inference turn. There is no mechanism to:

1. **Hide** a skill that should not appear in the catalog (e.g. `disable-model-invocation`).
2. **Dynamically promote/demote** skills from plugins, tools, or configuration at runtime.

### Alignment with the agentskills specification

Awaken aligns with the [agentskills spec](https://agentskills.io/specification)
**progressive-disclosure** model: a skill's `name` + `description` are surfaced to the
agent at startup for all skills, the full `SKILL.md` loads on activation, and resources
load on demand; the agent (model) decides when to use a skill. The spec's frontmatter is
`name`, `description`, `license`, `compatibility`, `metadata`, `allowed-tools`.

The visibility/invocation fields this ADR relies on are **Awaken extensions**, not part
of the agentskills core spec:

- `disable-model-invocation` — hide from the model catalog and block model invocation.
- `user-invocable` — controls the `/skill-name` user slash path.
- `paths` — parsed but **inert**; there is no path/glob conditional activation in the spec.

This ADR therefore gates catalog visibility **only** on `disable-model-invocation`
(see D1/D3).

The permission system (`awaken-ext-permission`) already demonstrates a clean
mechanism-policy separation: `PermissionPolicy` (thread-scoped) + `PermissionOverrides`
(run-scoped) → `evaluate_tool_permission()` → Allow/Deny/Ask. The deferred-tools
extension follows the same pattern with a `DeferralState` mechanism and declarative
config classification (`resolve_mode`) → Eager/Deferred.

Skill visibility should follow this established pattern rather than introducing
ad-hoc filtering logic.

## Decision

### D1: Extend `SkillFrontmatter` and `SkillMeta` with visibility-relevant fields

New optional frontmatter fields (all backward-compatible):

| Field | Type | Default | Purpose |
|-------|------|---------|---------|
| `when-to-use` | `Option<String>` | None | Hint for LLM catalog |
| `arguments` | `Option<Vec<{name, description?, required?}>>` | None | Formal parameter definitions |
| `argument-hint` | `Option<String>` | None | Free-text argument hint |
| `user-invocable` | `Option<bool>` | true | Whether `/skill-name` works |
| `disable-model-invocation` | `Option<bool>` | false | Hide from LLM catalog |
| `model` | `Option<String>` | None | Model override on activation |
| `context` | `Option<"inline"\|"fork">` | "inline" | Execution mode |
| `paths` | `Option<String>` | None | Glob patterns; **non-standard, parsed but inert** (does not gate visibility) |

`paths` is retained as parsed metadata for forward compatibility and possible
non-standard tooling, but it is **not** part of the agentskills spec and has no
effect on catalog visibility or activation.

Remove `deny_unknown_fields` from `SkillFrontmatter` serde attribute to allow
forward-compatible extension without breaking existing SKILL.md files.

### D2: Introduce `SkillVisibility` state key (run-scoped)

```
SkillVisibility = Visible | Hidden

SkillVisibilityAction:
  Show(id)               — explicit runtime override → visible
  Hide(id)               — explicit runtime override → hidden
  ShowBatch(Vec<id>)     — promote multiple skills (override → visible)
  SetBatch(Vec<(id,vis)>)  — overwrite entries (last-write-wins); explicit control
  SeedBatch(Vec<(id,vis)>) — run-start seed, INSERT-IF-ABSENT (never clobbers an
                             existing override)
```

The map records **explicit runtime overrides only**; absence means "no override",
resolved against metadata by `effective_visibility` (see D3/D4).

Run-scoped (`KeyScope::Run`), so visibility does not leak across runs (mirrors
`PermissionOverrides`). Merge is `Commutative` in the same sense as
`PermissionOverridesKey`: parallel batches may merge without conflict, per-skill
last-write-wins. `SeedBatch` is insert-if-absent so the run-start seed cannot
overwrite a runtime `Show`/`Hide` on re-activation, handoff, resume, or sub-agent
activation.

### D3: Declarative initial visibility, seeded at run start

Initial visibility is a declarative decision derived from skill metadata — not a
user-pluggable policy trait. A skill is `Hidden` when `model_invocable == false`
(frontmatter `disable-model-invocation: true`); otherwise `Visible`. `paths` is
**not** an input — there is no path-conditional hiding (the agentskills spec has no
such mechanism; see Context).

`SkillDiscoveryPlugin::on_activate` seeds **only the Hidden skills** at run start, via
`SkillVisibilityAction::SeedBatch` (insert-if-absent). Visible skills are deliberately
left unseeded: seeding them as explicit `Visible` would mask a later metadata change
(e.g. a registry hot-reload flipping a skill to non-invocable). With no seeded entry,
such skills always resolve through live metadata via `effective_visibility` (D4).

Initial visibility is derived **only from skill metadata**; `on_activate` does not
read `AgentSpec` (config-driven initial visibility is future scope, see D5). This
mirrors `awaken-ext-permission`, where policy is declarative rule data rather than a
code trait.

### D4: Catalog rendering filters by visibility state

`SkillDiscoveryPlugin::render_catalog()` reads `SkillVisibilityState` from the phase
context and excludes `Hidden` skills, resolving each skill through
`effective_visibility` (explicit Show/Hide wins, else the metadata policy — never
failing open). Also renders `when_to_use` into skill descriptions.

Catalog hiding is **discovery/noise control, not an invocation boundary**. The hard
guard is `model_invocable`: `SkillActivateTool` (the model's entry point) refuses to
activate a skill whose `disable-model-invocation` is set, regardless of whether it
was rendered.

This guard applies to the **model** path only. `user-invocable` (`/skill-name`) is a
separate invocation path that does not route through `SkillActivateTool`, so the
`model_invocable` guard cannot block a legitimate user invocation. The two controls
are orthogonal: `disable-model-invocation` governs the model; `user-invocable` governs
the slash path.

### D5: Dynamic control through existing action infrastructure

Any plugin, tool, or phase hook can schedule `SkillVisibilityAction`:

- **Plugin-driven**: Phase hooks mutate visibility state.
- **Tool-driven**: ToolSearch or custom tools can promote hidden skills.
- **Config-driven** (future, not yet implemented): Agent spec YAML rules set initial
  visibility. `on_activate` currently ignores `AgentSpec` and seeds from metadata only.

Note: path/glob conditional activation (hide a `paths` skill until a matching file is
touched) is **not** part of this design — it is absent from the agentskills spec. If a
non-standard extension ever wants it, it would build an `AfterToolExecute` hook on top
of this same action mechanism, but `paths` does not gate visibility today.

### D6: Parameter substitution in `activate()`

The default `Skill::activate()` implementation performs `${ARG_NAME}` substitution
against the provided arguments map, and `${SKILL_DIR}` against the skill's root
directory (FsSkill only).

## Consequences

- **Catalog noise reduction**: Skills with `disable-model-invocation` no longer
  consume context budget. Other skills (including `paths`-bearing ones) are surfaced
  by description per the agentskills progressive-disclosure model.
- **Extensibility**: The mechanism (state key + `SkillVisibilityAction`) is the
  extension surface — tools, plugins, and config drive visibility changes at
  runtime without changing the catalog rendering mechanism.
- **Consistency**: Follows the same mechanism-policy pattern as permission and
  deferred-tools, reducing cognitive load for contributors.
- **Backward compatibility**: All new frontmatter fields are optional; removing
  `deny_unknown_fields` allows older parsers to coexist with newer SKILL.md files.
