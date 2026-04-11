# 使用 Skills 子系统

当你希望 agent 在运行时发现、激活并按需加载技能包（指令、资源、脚本）时，使用本页。

## 前置条件

- 已有可运行的 awaken runtime
- `awaken` 启用了 `skills`

```toml
[dependencies]
awaken = { package = "awaken-agent", version = "0.1", features = ["skills"] }
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## 步骤

1. 创建技能目录：

```text
skills/
  my-skill/
    SKILL.md
  another-skill/
    SKILL.md
```

示例 `skills/my-skill/SKILL.md`：

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

2. 发现文件系统技能：

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

3. 也可以嵌入编译期技能：

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

4. 接入 runtime：

```rust,ignore
use std::sync::Arc;
use awaken::engine::GenaiExecutor;
use awaken::ext_skills::SkillSubsystem;
use awaken::registry::ModelBinding;
use awaken::registry_spec::AgentSpec;
use awaken::{AgentRuntimeBuilder, Plugin};

let subsystem = SkillSubsystem::new(registry);
let mut agent_spec = AgentSpec::new("skills-agent")
    .with_model_id("gpt-4o-mini")
    .with_system_prompt("Discover and activate skills when specialized help is useful.")
    .with_hook_filter("skills-discovery")
    .with_hook_filter("skills-active-instructions");
agent_spec.plugin_ids.extend([
    "skills-discovery".into(),
    "skills-active-instructions".into(),
]);

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

`plugin_ids` 会加载两个 skills 插件；hook filter 让该 agent 启用 discovery
和 active instruction 注入。

`SkillDiscoveryPlugin` 会把技能目录注入到上下文中，并注册三个工具：

| Tool | 用途 |
|------|------|
| `skill` | 按名称激活技能 |
| `load_skill_resource` | 读取技能资源或引用资料 |
| `skill_script` | 运行技能脚本 |

## 验证

1. 运行 agent，让它列出可用 skills
2. LLM 应该能看到技能目录，并调用 `skill` 激活某个技能
3. 激活后，后续推理会自动注入该 skill 的指令

## 常见错误

| 错误 | 原因 | 修复 |
|---|---|---|
| 没发现技能 | 目录结构不对 | 每个技能必须位于子目录里，并有 `SKILL.md` |
| `SkillMaterializeError` | frontmatter 无效 | 至少提供 `name` 和 `description` |
| skill tools 不可用 | 相关插件未注册 | 同时注册 discovery 和 active-instructions 两个插件 |
| feature 不存在 | Cargo 没开 `skills` | 在 `Cargo.toml` 中启用 |

## 相关示例

- `crates/awaken-ext-skills/tests/`

## 关键文件

- `crates/awaken-ext-skills/src/lib.rs`
- `crates/awaken-ext-skills/src/registry.rs`
- `crates/awaken-ext-skills/src/plugin/subsystem.rs`
- `crates/awaken-ext-skills/src/plugin/discovery.rs`
- `crates/awaken-ext-skills/src/embedded.rs`
- `crates/awaken-ext-skills/src/tools.rs`

## 相关

- [添加 Tool](./add-a-tool.md)
- [添加 Plugin](./add-a-plugin.md)
- [使用 MCP Tools](./use-mcp-tools.md)
