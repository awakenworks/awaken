---
title: "通过配置调优 Agent 行为"
description: "当同一个服务二进制需要承载多个 Agent 配置、切换模型绑定，或在不改 Rust 代码的情况下调优插件行为时，使用托管配置。新增 Tool、新增 Plugin、自定义 provider factory 仍然放在代码里；provider、model、agent、MCP server 和类型化 section 值放在配置里。"
---

当同一个服务二进制需要承载多个 Agent 配置、切换模型绑定，或在不改 Rust 代码的情况下调优插件行为时，使用托管配置。新增 Tool、新增 Plugin、自定义 provider factory 仍然放在代码里；provider、model、agent、MCP server 和类型化 section 值放在配置里。

本文假设服务端已经把 `ConfigStore` 接入 `AppState`，并且要引用的插件已经注册到 runtime plugin registry。

## 配置层级

| 层级 | 位置 | 用途 |
|---|---|---|
| Provider | `/v1/config/providers/{id}` | Adapter、API key 来源、base URL、timeout |
| Model binding | `/v1/config/models/{id}` | 稳定 model id -> provider id + 上游模型名 |
| Agent | `/v1/config/agents/{id}` | Prompt、model binding、轮数、工具、插件、上下文策略 |
| MCP server | `/v1/config/mcp-servers/{id}` | 外部 MCP server 连接 |
| Plugin section | `AgentSpec.sections` | 按 `PluginConfigKey::KEY` 归档的每 Agent 类型化配置 |
| Runtime code | `AgentRuntimeBuilder` | 注册工具、provider factory、插件、backend |

托管配置 runtime 支持的 provider adapter 包括：`anthropic`、`openai`、`openai_resp`、`deepseek`、`gemini`、`ollama`、`cohere`、`together`、`fireworks`、`groq`、`xai`、`zai`、`bigmodel`、`aliyun`、`mimo`、`nebius`。

## 解析模型

本地 Agent 执行通过稳定注册表 ID 解析 model 与 provider：

```text
AgentSpec.model_id
  -> ModelBindingSpec { provider_id, upstream_model }
  -> ProviderSpec { adapter, api_key, base_url, timeout_secs }
  -> LlmExecutor
```

`AgentSpec.model_id` 不是上游 provider 模型名。它是 Agent 和客户端使用的稳定 model binding id。`ModelBindingSpec.upstream_model` 才是发送给 provider API 的模型字符串。

配置写入会先编译成候选 registry snapshot 并完成校验，然后再发布。新的 run 使用最新发布的 snapshot。已经开始的 run 保持启动时绑定的 snapshot。

Endpoint-backed Agent 会跳过本地 provider、model、plugin 和 tool 解析链，改由选中的远程 backend 执行其 `endpoint` 配置。

## 最小托管配置

创建或更新 provider。省略 `api_key` 时，provider adapter 会使用自身环境变量。将 `api_key` 设为 `null` 或 `""` 会清除已保存 key。

```bash
curl -sS -X PUT http://localhost:3000/v1/config/providers/anthropic-prod \
  -H 'content-type: application/json' \
  -d '{
    "id": "anthropic-prod",
    "adapter": "anthropic",
    "base_url": null,
    "timeout_secs": 300
  }'
```

把稳定 model id 绑定到该 provider：

```bash
curl -sS -X PUT http://localhost:3000/v1/config/models/research-default \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-default",
    "provider_id": "anthropic-prod",
    "upstream_model": "claude-sonnet-4-20250514"
  }'
```

创建使用该 model binding 的 Agent：

```bash
curl -sS -X PUT http://localhost:3000/v1/config/agents/research-assistant \
  -H 'content-type: application/json' \
  -d '{
    "id": "research-assistant",
    "model_id": "research-default",
    "system_prompt": "You help with source-grounded research. Ask before using destructive tools.",
    "max_rounds": 12,
    "reasoning_effort": "medium",
    "plugin_ids": ["permission"],
    "allowed_tools": ["web_search", "read_document", "summarize"],
    "context_policy": {
      "max_context_tokens": 120000,
      "max_output_tokens": 8192,
      "min_recent_messages": 8,
      "enable_prompt_cache": true,
      "autocompact_threshold": 90000,
      "compaction_mode": "keep_recent_raw_suffix",
      "compaction_raw_suffix_messages": 2
    }
  }'
```

## 用 sections 调优

`AgentSpec.sections` 承载类型化插件或解析器配置。key 是 `PluginConfigKey::KEY` 声明的稳定字符串；value 必须匹配读取该 key 的消费者 schema。

```json
{
  "sections": {
    "retry": {
      "max_retries": 2,
      "fallback_upstream_models": ["claude-3-haiku"],
      "backoff_base_ms": 500
    },
    "permission": {
      "default_behavior": "ask",
      "rules": [
        { "tool": "read_document", "behavior": "allow" },
        { "tool": "web_search", "behavior": "ask" },
        { "tool": "delete_*", "behavior": "deny" }
      ]
    },
    "reminder": {
      "rules": [
        {
          "tool": "Edit(file_path ~ '*.toml')",
          "output": "any",
          "message": {
            "target": "suffix_system",
            "content": "You edited a TOML file. Run cargo check before finishing."
          }
        }
      ]
    },
    "generative-ui": {
      "catalog_id": "https://a2ui.org/specification/v0_8/standard_catalog_definition.json",
      "examples": "Use compact components for status summaries and forms."
    },
    "deferred_tools": {
      "enabled": true,
      "default_mode": "deferred",
      "rules": [
        { "tool": "summarize", "mode": "eager" }
      ],
      "beta_overhead": 1136.0
    },
    "compaction": {
      "summarizer_system_prompt": "You are a conversation summarizer. Preserve decisions, facts, tool results, and unresolved tasks.",
      "summarizer_user_prompt": "Summarize the following conversation:\n\n{messages}",
      "summary_max_tokens": 1024,
      "summary_model": "claude-3-haiku",
      "min_savings_ratio": 0.3
    }
  }
}
```

常用 key：

| Key | 消费方 | 说明 |
|---|---|---|
| `retry` | Resolver | 重试和同 provider 内的 fallback upstream models。 |
| `permission` | Permission plugin | 默认 allow/ask/deny 行为和有序工具规则。 |
| `reminder` | Reminder plugin | 按工具和输出匹配并注入 system 或 conversation context 的规则。 |
| `generative-ui` | Generative UI plugin | A2UI catalog id、examples 或完整 prompt instructions。 |
| `deferred_tools` | Deferred tools plugin | 决定哪些工具 schema 保持 eager，哪些按需加载。 |
| `compaction` | Context compaction plugin | 摘要 prompt、摘要模型和可接受的节省阈值。 |

`context_policy` 是 `AgentSpec` 顶层字段，不是 section。设置它会启用上下文 transform 和上下文压缩。可选的 `compaction` section 只用于调优压缩时使用的 summarizer。

`plugin_ids` 和 section key 不同。`plugin_ids` 使用插件注册表 ID，例如 `permission`、`reminder`、`ext-deferred-tools`。`deferred_tools` section key 是 deferred tools 插件的配置 key。
使用插件 section 时，需要保证对应插件 id 已经写入 `plugin_ids`。`retry` 由 resolver 读取；当
`context_policy` 启用内置上下文压缩插件时，`compaction` 可用。

有些插件也接受构造期默认值。例如 `ReminderPlugin::new(rules)` 会安装全局默认 reminder 规则；每个 Agent 的 `reminder` section 则通过 `ReminderConfigKey` 校验，并在运行时追加到默认规则之后。全局基线用构造期默认值，单个 Agent 的调优用 `AgentSpec.sections`。

常规 Agent 应保持 `active_hook_filter` 为空。非空 filter 会禁用未列入 descriptor 名称的插件 hooks、插件工具和 request transforms；它主要用于有意收窄某个 Agent 的活跃行为。

## 调优流程

1. 先确定稳定的 `providers`、`models` 和 `agents` id。客户端按 agent id 调用，Agent 按 model binding id 引用模型。
2. 用 model binding 切换上游模型名。需要切换 provider 时，使用另一个 binding。
3. 用 `system_prompt`、`max_rounds`、`max_continuation_retries`、`reasoning_effort` 和 `context_policy` 调整整体循环行为。
4. 用 `allowed_tools` 和 `excluded_tools` 限制可见工具。
5. 用 `permission` 规则处理运行时 allow/ask/deny 决策。
6. 用 `retry` fallback upstream models 增强同 provider 内的韧性。
7. 只有需要对应行为时，才添加 `reminder`、`generative-ui`、`deferred_tools` 和 `compaction` section。
8. 通过 `/v1/config/*` 发布配置；写入成功后，新建 run 才会使用新的 snapshot。

## 兼容性规则

- 对外保持 `AgentSpec.id`、`ModelBindingSpec.id` 和 `ProviderSpec.id` 稳定。
- 使用 canonical 字段：`model_id`、`provider_id`、`upstream_model`、`fallback_upstream_models`。旧的 `model`、`provider`、`fallback_models` 不是托管配置字段。
- 将 `InferenceOverride.upstream_model` 视为同 provider 内的覆盖。它不会重新解析 `AgentSpec.model_id`，也不能切换 provider executor。
- 生成配置前查询 `/v1/config/{namespace}/$schema`，并通过 `/v1/capabilities` 查看插件 `config_schemas`。`AgentSpec`、`ModelBindingSpec` 和多个 section 类型会拒绝未知字段。
- 只要插件已注册且 section value 匹配 schema，新增 section 属于兼容变更。不合法 section 会在 runtime snapshot 发布前失败。
- 移除 plugin id 但保留对应 section 不会激活该插件；未被消费的 section key 会作为可能的拼写错误记录日志。
- 已经开始的 run 保持启动时的 snapshot。验证配置变更时，应在写入成功后新建 run。

## 相关

- [Provider 与 Model 配置](/reference/provider-model-config/)
- [配置](/reference/config/)
- [HTTP API](/reference/http-api/)
- [启用工具权限 HITL](/enable-tool-permission-hitl/)
- [使用 Reminder 插件](/use-reminder-plugin/)
- [使用延迟加载工具](/use-deferred-tools/)
- [优化上下文窗口](/optimize-context-window/)
