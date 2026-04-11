# Use Skills Subsystem

Use this when you want agents to discover and activate skill packages at runtime, loading instructions, resources, and scripts on demand.

## Prerequisites

- A working awaken agent runtime (see [First Agent](../tutorials/first-agent.md))
- Feature `skills` enabled on the `awaken` crate

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["skills"] }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Steps

1. Create skill directories.

Each skill lives in a directory with a `SKILL.md` file containing YAML frontmatter:

```text
skills/
  my-skill/
    SKILL.md
  another-skill/
    SKILL.md
```

Example `skills/my-skill/SKILL.md`:

```markdown
---
name: my-skill
description: Helps with data analysis tasks
allowed-tools: read_file web_search
---
# My Skill

When this skill is active, focus on data analysis.
Use read_file to load datasets and web_search for reference material.
```

2. Discover filesystem skills.

```rust,ignore
use std::sync::Arc;
use awaken::ext_skills::{FsSkill, InMemorySkillRegistry, SkillSubsystem};

let result = FsSkill::discover("./skills").expect("skill discovery failed");
for warning in &result.warnings {
    eprintln!("skill warning: {warning:?}");
}

let skills = FsSkill::into_arc_skills(result.skills);
let registry = Arc::new(InMemorySkillRegistry::from_skills(skills));
```

3. Use embedded skills (alternative).

   For compile-time embedded skills, use `EmbeddedSkill`:

```rust,ignore
use std::sync::Arc;
use awaken::ext_skills::{EmbeddedSkill, EmbeddedSkillData, InMemorySkillRegistry};

const SKILL_MD: &str = "\
---
name: builtin-skill
description: A compiled-in skill
allowed-tools: read_file
---
# Builtin Skill

Follow these instructions when active.
";

let skill = EmbeddedSkill::new(&EmbeddedSkillData {
    skill_md: SKILL_MD,
    references: &[],
    assets: &[],
}).expect("valid skill");

let registry = Arc::new(InMemorySkillRegistry::from_skills(vec![Arc::new(skill)]));
```

4. Wire into the runtime.

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_skills::SkillSubsystem;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, Plugin};

let subsystem = SkillSubsystem::new(registry);
let agent_spec = AgentSpec::new("skills-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("Discover and activate skills when specialized help is useful.")
    .with_hook_filter("skills-discovery")
    .with_hook_filter("skills-active-instructions");

let runtime = AgentRuntimeBuilder::new()
    .with_provider("openai", Arc::new(GenaiExecutor::new()))
    .with_model_binding(
        "gpt-4o-mini",
        ModelBinding {
            provider_id: "openai".into(),
            upstream_model: "gpt-4o-mini".into(),
        },
    )
    .with_agent_spec(agent_spec)
    .with_plugin(
        "skills-discovery",
        Arc::new(subsystem.discovery_plugin()) as Arc<dyn Plugin>,
    )
    .with_plugin(
        "skills-active-instructions",
        Arc::new(subsystem.active_instructions_plugin()) as Arc<dyn Plugin>,
    )
    .build()
    .expect("failed to build runtime");
```

The `SkillDiscoveryPlugin` injects a skills catalog into the LLM context before inference and registers three tools:

| Tool | Purpose |
|------|---------|
| `skill` | Activate a skill by name |
| `load_skill_resource` | Load a skill resource or reference |
| `skill_script` | Run a skill script |

## Verify

1. Run the agent and ask it to list available skills.
2. The LLM should see the skills catalog in its context and use the `skill` tool to activate one.
3. After activation, the `ActiveSkillInstructionsPlugin` injects the skill instructions into subsequent inference calls.

## Common Errors

| Symptom | Cause | Fix |
|---------|-------|-----|
| No skills discovered | Wrong directory path | Ensure each skill has a `SKILL.md` in a subdirectory |
| `SkillMaterializeError` | Invalid YAML frontmatter | Check that `name` and `description` fields are present in `SKILL.md` |
| Skill tools not available | Plugin not registered | Register both `discovery_plugin()` and `active_instructions_plugin()` |
| Feature not found | Missing cargo feature | Enable `features = ["skills"]` in `Cargo.toml` |

## Related Example

- `crates/awaken-ext-skills/tests/`

## Key Files

| Path | Purpose |
|------|---------|
| `crates/awaken-ext-skills/src/lib.rs` | Module root and public re-exports |
| `crates/awaken-ext-skills/src/registry.rs` | `FsSkill`, `InMemorySkillRegistry`, `SkillRegistry` trait |
| `crates/awaken-ext-skills/src/plugin/subsystem.rs` | `SkillSubsystem` facade |
| `crates/awaken-ext-skills/src/plugin/discovery.rs` | `SkillDiscoveryPlugin` |
| `crates/awaken-ext-skills/src/embedded.rs` | `EmbeddedSkill` for compile-time skills |
| `crates/awaken-ext-skills/src/tools.rs` | Skill tool implementations |

## Related

- [Add a Tool](./add-a-tool.md)
- [Add a Plugin](./add-a-plugin.md)
- [Use MCP Tools](./use-mcp-tools.md)
