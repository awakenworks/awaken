# ADR-0020: Skill Visibility — Mechanism-Policy Separation

- **Status**: Accepted
- **Date**: 2026-04-02
- **Depends on**: ADR-0015 (Plugin Activation), ADR-0016 (Tool Interception Pipeline)

## Context

The skill subsystem (`awaken-ext-skills`) discovers skills from filesystem, MCP, and
embedded sources and unconditionally injects **all** of them into the LLM catalog
before every inference turn. There is no mechanism to:

1. **Hide** a skill that should not appear in the catalog (e.g. `disable-model-invocation`).
2. **Conditionally show** a skill only when the user touches matching files (`paths` patterns).
3. **Dynamically promote/demote** skills from plugins, tools, or configuration at runtime.

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
| `paths` | `Option<String>` | None | Comma/newline-separated glob patterns |

Remove `deny_unknown_fields` from `SkillFrontmatter` serde attribute to allow
forward-compatible extension without breaking existing SKILL.md files.

### D2: Introduce `SkillVisibility` state key (run-scoped)

```
SkillVisibility = Visible | Hidden

SkillVisibilityAction:
  Show(id)              — make a skill visible
  Hide(id)              — hide a skill
  ShowBatch(Vec<id>)    — promote multiple skills
  SetBatch(Vec<(id, vis)>) — set initial visibility for all skills
```

Run-scoped (`KeyScope::Run`) with commutative merge (set-last-write-wins per skill ID).
This mirrors `PermissionOverrides` scoping — visibility does not leak across runs.

### D3: Declarative initial visibility, seeded at run start

Initial visibility is a declarative decision derived from skill metadata — not a
user-pluggable policy trait. A skill starts `Hidden` when `model_invocable ==
false` or `paths` is non-empty (conditional skills start hidden, promoted on file
match); otherwise `Visible`.

`SkillDiscoveryPlugin::on_activate` evaluates this against the registry snapshot
and seeds `SkillVisibilityState` via `SkillVisibilityAction::SetBatch` at run
start. Subsequent changes come through actions (tools, plugins, config). This
mirrors `awaken-ext-permission`, where policy is declarative rule data rather than
a code trait.

### D4: Catalog rendering filters by visibility state

`SkillDiscoveryPlugin::render_catalog()` reads `SkillVisibilityState` from the phase
context and excludes `Hidden` skills. Also renders `when_to_use` into skill
descriptions.

### D5: Dynamic control through existing action infrastructure

Any plugin, tool, or phase hook can schedule `SkillVisibilityAction`:

- **Config-driven**: Agent spec YAML rules set initial visibility.
- **Plugin-driven**: Phase hooks mutate visibility state.
- **Tool-driven**: ToolSearch or custom tools can promote hidden skills.
- **Conditional activation**: An `AfterToolExecute` hook can match file paths against
  skill `paths` patterns and promote matching skills (future, not in this ADR scope).

### D6: Parameter substitution in `activate()`

The default `Skill::activate()` implementation performs `${ARG_NAME}` substitution
against the provided arguments map, and `${SKILL_DIR}` against the skill's root
directory (FsSkill only).

## Consequences

- **Catalog noise reduction**: Skills with `disable-model-invocation` or unfulfilled
  `paths` patterns no longer consume context budget.
- **Extensibility**: The mechanism (state key + `SkillVisibilityAction`) is the
  extension surface — tools, plugins, and config drive visibility changes at
  runtime without changing the catalog rendering mechanism.
- **Consistency**: Follows the same mechanism-policy pattern as permission and
  deferred-tools, reducing cognitive load for contributors.
- **Backward compatibility**: All new frontmatter fields are optional; removing
  `deny_unknown_fields` allows older parsers to coexist with newer SKILL.md files.
