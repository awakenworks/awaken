# Use Skills Subsystem

Use this when you want reusable file-backed skills (`SKILL.md`, references, scripts) as runtime tools/context.

## Prerequisites

- Skill directories containing `SKILL.md`.
- `tirea-extension-skills` available.

## Steps

1. Discover skills from filesystem.

```rust,ignore
use tirea::skills::FsSkill;

let discovered = FsSkill::discover("./skills")?;
let skills = FsSkill::into_arc_skills(discovered.skills);
```

2. Enable skills mode in builder.

```rust,ignore
use tirea::orchestrator::{AgentDefinition, AgentOsBuilder, SkillsConfig};

let os = AgentOsBuilder::new()
    .with_skills(skills)
    .with_skills_config(SkillsConfig {
        enabled: true,
        advertise_catalog: true,
        ..SkillsConfig::default()
    })
    .with_agent("assistant", AgentDefinition::new("deepseek-chat"))
    .build()?;
```

Config flags:

- `enabled`: registers skill tools (`skill`, `load_skill_resource`, `skill_script`)
- `advertise_catalog`: injects available-skills catalog into inference context

3. (Optional) use scope filters per run.

- `__agent_policy_allowed_skills`
- `__agent_policy_excluded_skills`

## Verify

- Resolved tools include `skill`, `load_skill_resource`, `skill_script`.
- Model receives available-skills context (when discovery mode is enabled).
- Activated skill resources/scripts are accessible in runtime.

## Common Errors

- Enabling skills mode without providing skills/registry.
- Tool id conflict with existing `skill` tool names.

## Related

- [Capability Matrix](../reference/capability-matrix.md)
- [Config](../reference/config.md)
- `crates/tirea-agentos/src/orchestrator/tests.rs`
