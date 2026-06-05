---
title: "用 Admin Assistant 构建 Agent"
description: "用自然语言描述你想要的 agent,内置 Admin Assistant 会替你起草并校验 spec,再由你在编辑器中发布。"
---

**Admin Assistant** 是一个内置 agent,把自然语言描述变成一份已校验的
`AgentSpec`:它读取平台能力、起草 spec、做校验,然后把草稿交给你审阅并发布。这是
从「意图」到「可用 agent」最快的路径,无需手动逐字填写编辑器。

## 前置条件

- 一个已接入 `ConfigStore`、可从控制台访问的 `awaken-server`。
- **至少配置并发布了一个 provider-backed 模型。** 在此之前 Admin Assistant 处于
  禁用状态(离线 `scripted` 模型不算)。请先配置一个 —— 见
  [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)的「测试 Provider」。

## 步骤

1. 点击右下角悬浮的 **Awaken** 气泡。若它显示警告图标,悬停查看提示:通常是
   「Configure a provider-backed model to enable the admin assistant」,面板会给出
   Providers/Models 的设置入口。

   <figure class="screenshot">
     <a href="/awaken/assets/admin-console/admin-assistant.png">
       <img src="/awaken/assets/admin-console/admin-assistant.png" alt="Admin Assistant 面板:对其能力的说明、常见 agent 类型的建议 chips,以及「Describe your agent or ask about config」输入框。" loading="lazy" />
     </a>
     <figcaption>Admin Assistant —— 描述你的 agent,或点一个起始提示。</figcaption>
   </figure>

2. 在输入框(「Describe your agent」)里描述你的需求 —— id、模型、行为,以及需要的
   工具或委派。例如:

   > 创建一个 id 为 `concierge`、模型为 `default` 的 agent:友好的接待助手,介绍产品
   > 能做什么,回答简短。直接创建并校验。

3. 按 **Enter**。助手会流式输出推理,并依次调用它的工具:
   - `admin_get_platform_capabilities` —— 读取经脱敏的模型、工具、provider、插件、
     MCP 快照。
   - `admin_create_agent_draft` —— 根据意图起草规范化的 `AgentSpec`。
   - `admin_validate_agent` —— 用与 `POST /v1/config/agents/validate` 相同的检查
     校验,返回 `ok` 或错误。
4. **审阅并发布。** 按设计,助手**没有发布工具** —— 它停在「已校验的草稿」并告诉你
   下一步。打开该 agent 的编辑器,确认字段,点击 **Save & Publish**。随后它出现在
   **Agents** 列表,并在下一个请求时生效。

助手运行在 `POST /v1/admin/assistant/runs`(AI SDK 消息格式的 SSE 流),使用它自己
锁定的系统提示和你为它配置的模型。

## 调优助手

助手的行为由一段 policy prompt 和一个模型绑定决定:

- `GET` / `PUT /v1/admin/assistant/config` —— 设置 `model_id` 与 `policy_prompt`
  (≤8 KB,追加在锁定指令之后)。写入采用基于 revision 的乐观锁(`409` 表示有人已
  改动)。
- 若不设置 `model_id`,助手会自动选择第一个可用的 provider-backed 模型。

## 注意

- **审批关口。** 草稿绝不会自动发布 —— 发布始终经由编辑器(或
  `POST /v1/config/agents`)。这保证流程中始终有人参与。
- **共享 `default`。** 构建过程中助手可能把共享的 `default` provider/模型重指。若你
  在别处依赖 `default` 跑实时请求,事后请重新断言(demo 录制就是在 eval 前这么做的)。
- **仅管理员可见。** 助手不会出现在非管理员的能力视图中。

## 相关

- [使用管理控制台](/awaken/zh-cn/how-to/use-admin-console/)
- [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/)
- [采集数据集并运行评测](/awaken/zh-cn/how-to/capture-a-dataset-and-run-an-eval/)
