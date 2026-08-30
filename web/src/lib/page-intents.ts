import { NAV } from "./navigation/paths";

export interface PageIntent {
  readonly title: string;
  readonly titleZh: string;
  readonly description: string;
  readonly descriptionZh: string;
  readonly outcome: string;
  readonly outcomeZh: string;
}

export const TOP_LEVEL_PAGE_INTENTS: Record<string, PageIntent> = {
  overview: {
    title: "Set up, run, and verify an Agent",
    titleZh: "完成 Agent 配置、运行与验证",
    description: "Use the readiness checks and five-step journey to move from a connected model to a verified Session and a runnable application integration.",
    descriptionZh: "按照就绪检查和五步路径，从连接模型开始，完成真实会话验证，再接入可运行的应用。",
    outcome: "Start with the first readiness item that needs attention.",
    outcomeZh: "从第一个“需要处理”的就绪项开始。",
  },
  agents: {
    title: "Create, adopt, or connect an Agent",
    titleZh: "创建、采用或接入 Agent",
    description: "Run Agent applications behind one durable, self-hostable service. Each publication fixes the instructions, model, capabilities, resources, permissions, and execution policy used by new Sessions.",
    descriptionZh: "在一个持久、可自托管的服务中运行 Agent 应用。每次发布都会固定新会话使用的指令、模型、能力、资源、权限与执行策略。",
    outcome: "Start with the work the Agent must finish. Test the draft in Preview, then publish only the version your team has reviewed.",
    outcomeZh: "先说明 Agent 必须完成的工作，在预览中验证草稿，再发布团队已经检查的版本。",
  },
  skills: {
    title: "Skills",
    titleZh: "技能",
    description: "Publish reusable instructions and supporting files that Agents can load when a task needs them. The Sandbox badge tells you whether execution is required.",
    descriptionZh: "发布可复用的指令与支持文件，让 Agent 在任务需要时加载。Sandbox 标记会说明该技能是否需要执行环境。",
    outcome: "Create a simple instruction-only Skill, or import a complete Skill folder.",
    outcomeZh: "可在线创建纯指令技能，也可导入完整技能目录。",
  },
  files: {
    title: "Files",
    titleZh: "文件",
    description: "Upload reusable input files for this Workspace. To make a file available during a run, bind it to an Agent under Build → Knowledge.",
    descriptionZh: "上传当前工作区可复用的输入文件。若要在运行中使用，请在 Agent 的“构建 → 知识”中绑定。",
    outcome: "Upload an input, then attach it to the Agent that needs it.",
    outcomeZh: "先上传输入文件，再绑定到需要它的 Agent。",
  },
  memory: {
    title: "Memory",
    titleZh: "记忆",
    description: "Manage durable Memory Stores and Dream consolidation runs. Stores keep editable content across Sessions; Dreams create separate outputs for review.",
    descriptionZh: "管理跨会话保存的记忆库和 Dream 整理任务。记忆库内容可编辑；Dream 会生成独立输出供检查。",
    outcome: "Open a Store to edit content, review history, run Dreams, or configure automation.",
    outcomeZh: "打开记忆库即可编辑内容、查看历史、运行 Dream 或配置自动整理。",
  },
  sessions: {
    title: "Sessions",
    titleZh: "会话",
    description: "Give a published Agent real work, then keep its conversation, inputs, outputs, tool evidence, controls, and recovery state together in one durable Session.",
    descriptionZh: "把真实工作交给已发布的 Agent，并在一个持久会话中保留对话、输入、输出、工具证据、控制与恢复状态。",
    outcome: "Create a Session after its Agent and model show as ready.",
    outcomeZh: "确认 Agent 和模型已就绪后，再创建会话。",
  },
  deployments: {
    title: "Deployments",
    titleZh: "部署",
    description: "Turn recurring work into a standing operation. Every scheduled or manual trigger creates an inspectable Session with its own result and evidence.",
    descriptionZh: "把反复发生的工作变成持续运行的任务。每次计划或手动触发都会创建一个可独立检查结果与证据的会话。",
    outcome: "Choose an Agent and schedule, then use Run once to verify the deployment.",
    outcomeZh: "选择 Agent 和计划后，先手动运行一次验证部署。",
  },
  artifacts: {
    title: "Artifacts",
    titleZh: "产物",
    description: "Review and download files produced by Sessions. Artifacts are read-only outputs; reusable inputs belong under Files.",
    descriptionZh: "查看和下载会话生成的文件。产物是只读输出；可复用输入应放在“文件”中。",
    outcome: "Open the producing Session when you need the conversation and execution context.",
    outcomeZh: "需要对话与执行上下文时，请打开生成该产物的会话。",
  },
  environments: {
    title: "Environments",
    titleZh: "运行环境",
    description: "Control where Sessions run and which packages, network access, resource limits, and Sandbox timing they receive.",
    descriptionZh: "控制会话在哪里运行，以及可用的软件包、网络、资源限制和 Sandbox 创建时机。",
    outcome: "Use the local default for simple runs; create a dedicated Environment when isolation or packages differ.",
    outcomeZh: "简单运行使用本地默认环境；需要不同隔离或软件包时再新建专用环境。",
  },
  models: {
    title: "Models & providers",
    titleZh: "模型与供应商",
    description: "Connect a provider, verify its credential and endpoint, then test a real model response. Only active, executable models appear in Agent setup.",
    descriptionZh: "连接供应商，验证凭证与端点，再测试一次真实模型响应。只有可执行的活跃模型会出现在 Agent 配置中。",
    outcome: "Connect one provider and use Test before creating an Agent.",
    outcomeZh: "创建 Agent 前，请至少连接一个供应商并完成模型测试。",
  },
  mcp: {
    title: "MCP overview",
    titleZh: "MCP 概览",
    description: "See which MCP servers are referenced by Agent drafts and which Agents use them. MCP servers are configured inside each Agent, not on this page.",
    descriptionZh: "查看 Agent 草稿引用的 MCP 服务器及其使用者。MCP 服务器在各 Agent 内配置，本页只做汇总。",
    outcome: "Open the owning Agent to add, change, or remove an MCP server.",
    outcomeZh: "需要新增、修改或移除 MCP 服务器时，请打开对应 Agent。",
  },
  protocols: {
    title: "API & protocols",
    titleZh: "API 与协议",
    description: "Choose how trusted services, applications, and other agents call the same published Agent runtime, with the correct token type for each client.",
    descriptionZh: "为可信服务、应用或其他 Agent 选择调用同一运行时的协议，并使用与客户端类型匹配的令牌。",
    outcome: "Prove the Agent in a Session first, then copy the integration pattern for your client.",
    outcomeZh: "先在会话中验证 Agent，再为你的客户端采用对应集成方式。",
  },
  webhooks: {
    title: "Webhooks",
    titleZh: "Webhooks",
    description: "Send signed lifecycle notifications from Awaken to a trusted backend. Each endpoint owns its event filters, one-time signing secret, and durable delivery state.",
    descriptionZh: "由 Awaken 向可信后端发送已签名的生命周期通知。每个端点独立管理事件筛选、一次性签名密钥与持久投递状态。",
    outcome: "Create an endpoint, copy its secret once, verify a real delivery, and monitor failures here.",
    outcomeZh: "创建端点并复制一次性密钥，验证真实投递，再在此监控失败状态。",
  },
  a2a: {
    title: "A2A federation",
    titleZh: "A2A 联邦",
    description: "Inspect this deployment's published Agent Card and the endpoints that let other A2A agents call it.",
    descriptionZh: "检查当前部署发布的 Agent Card，以及其他 A2A Agent 调用它的入口。",
    outcome: "Load the public Agent Card and verify its advertised protocol and message endpoints.",
    outcomeZh: "加载公开 Agent Card，验证其声明的协议与消息入口。",
  },
  access: {
    title: "Access",
    titleZh: "访问控制",
    description: "Create and revoke Workspace-scoped service API keys for trusted backends. A new key is shown only once and must never be placed in browser code.",
    descriptionZh: "为可信后端创建或吊销工作区范围的 Service API Key。新 Key 只显示一次，禁止放入浏览器代码。",
    outcome: "For the Managed Agents SDK, create a dedicated expiring key that can start Sessions, copy it once, then continue to API & protocols.",
    outcomeZh: "Managed Agents SDK 请创建可启动 Session 的专用限时 Key，完成一次性复制后继续前往“API 与协议”。",
  },
  vaults: {
    title: "Runtime secrets",
    titleZh: "运行时凭证",
    description: "Store credentials used by tools, MCP servers, and Sandbox processes. Model provider keys are managed separately under Models & providers.",
    descriptionZh: "保存工具、MCP 服务器和 Sandbox 进程使用的凭证。模型供应商 Key 请在“模型与供应商”中管理。",
    outcome: "Create a clearly named Vault category, then add credentials of the matching runtime type.",
    outcomeZh: "先创建分类清晰的 Vault，再添加与用途匹配的运行时凭证。",
  },
  settings: {
    title: "Settings",
    titleZh: "设置",
    description: "Review this Workspace's configuration scope and jump to the pages that own model supply, execution, integrations, notifications, and access.",
    descriptionZh: "确认当前工作区的配置范围，并前往负责模型供给、执行、集成、通知与访问控制的页面。",
    outcome: "Use these links as configuration shortcuts; the settings themselves remain on their owning pages.",
    outcomeZh: "这里提供配置入口；具体设置仍由各自页面负责。",
  },
};

const DYNAMIC_PAGE_INTENTS: Array<[RegExp, PageIntent]> = [
  [/^\/w\/[^/]+\/agents\/new$/, {
    title: "Create Agent", titleZh: "创建 Agent",
    description: "Start with the outcome this Agent must deliver. Choose its model and Environment, test the complete draft without saving it, then add only the authority the work needs.",
    descriptionZh: "先说明这个 Agent 必须交付的结果。选择模型与运行环境，在不保存草稿的情况下完成测试，再只授予工作真正需要的权限。",
    outcome: "Preview the decision or artifact first. Publish only after the review matches the intended responsibility boundary.", outcomeZh: "先预览决策或产物；确认评审内容符合预期责任边界后再发布。",
  }],
  [/^\/w\/[^/]+\/agents\/[^/]+$/, {
    title: "Edit Agent", titleZh: "编辑 Agent",
    description: "Change the draft, validate it, test it, and publish only after the review shows the intended differences.",
    descriptionZh: "修改草稿、验证并试运行；确认发布差异符合预期后再发布。",
    outcome: "Draft edits do not change existing Sessions or the published version until you publish.", outcomeZh: "草稿在发布前不会影响已有会话或当前发布版本。",
  }],
  [/^\/w\/[^/]+\/sessions\/[^/]+$/, {
    title: "Session details", titleZh: "会话详情",
    description: "Continue or interrupt the run, then inspect the exact inputs, artifacts, integrations, and trace behind its conversation.",
    descriptionZh: "继续或中断运行，并检查对话背后的确切输入、产物、集成和追踪。",
    outcome: "Use Chat for the task; use the other tabs when you need evidence or diagnosis.", outcomeZh: "在“对话”中执行任务；需要证据或诊断时再查看其他标签。",
  }],
  [/^\/w\/[^/]+\/memory\/dreams\/[^/]+$/, {
    title: "Dream details", titleZh: "Dream 详情",
    description: "Review what a Dream used, what it produced, and how the output differs from its source before attaching that output to an Agent.",
    descriptionZh: "在将输出绑定到 Agent 前，检查 Dream 使用的证据、生成结果及其与来源的差异。",
    outcome: "Open the output Store and review its content before choosing Use in Agent.", outcomeZh: "选择“用于 Agent”前，请先打开输出记忆库检查内容。",
  }],
  [/^\/w\/[^/]+\/assistant$/, {
    title: "Ask the Console Assistant", titleZh: "询问 Console 助手",
    description: "Ask about any Console concept or workflow, diagnose what blocks you, or describe an Agent or Environment you want configured.",
    descriptionZh: "可询问任何 Console 概念或流程、诊断当前阻碍，或描述需要配置的 Agent 与运行环境。",
    outcome: "Ask naturally; the Assistant uses the current page as context and keeps activation under your review.", outcomeZh: "直接用自然语言提问；助手会使用当前页面上下文，所有生效操作仍由你审阅。",
  }],
  [/^\/w\/[^/]+\/credentials$/, {
    title: "Model credential sources", titleZh: "模型凭证来源",
    description: "Inspect the secret-free status of model credentials and add a Claude Code setup token. Provider API keys are normally created from Models & providers.",
    descriptionZh: "检查模型凭证的非敏感状态，并可添加 Claude Code Setup Token。供应商 API Key 通常从“模型与供应商”创建。",
    outcome: "Return to Models & providers to connect or test a model.", outcomeZh: "连接或测试模型请返回“模型与供应商”。",
  }],
  [/^\/w\/[^/]+\/dashboard$/, { title: "Run dashboard", titleZh: "运行仪表盘", description: "Inspect aggregate run health when the deployment exposes this optional capability.", descriptionZh: "当部署提供该可选能力时，查看整体运行健康状况。", outcome: "Use Sessions for individual run evidence.", outcomeZh: "单次运行证据请前往“会话”。" }],
  [/^\/w\/[^/]+\/audit-log$/, { title: "Audit log", titleZh: "审计日志", description: "Review management changes when the deployment exposes the audit-log capability.", descriptionZh: "当部署提供审计日志能力时，检查管理配置变更。", outcome: "Filter by actor and action in the dedicated UI when available.", outcomeZh: "专用界面可用后，可按操作者与操作筛选。" }],
  [/^\/w\/[^/]+\/datasets$/, { title: "Evaluation datasets", titleZh: "评测数据集", description: "Manage reusable evaluation inputs when the deployment enables evaluation APIs.", descriptionZh: "当部署启用评测 API 时，管理可复用的评测输入。", outcome: "Create an Eval run from a reviewed dataset.", outcomeZh: "基于已检查的数据集创建评测运行。" }],
  [/^\/w\/[^/]+\/eval-runs$/, { title: "Evaluation runs", titleZh: "评测运行", description: "Compare Agent behavior against evaluation datasets when this optional capability is enabled.", descriptionZh: "启用该可选能力后，可基于评测数据集比较 Agent 表现。", outcome: "Open failed cases and return to the Agent draft to improve them.", outcomeZh: "打开失败用例，并返回 Agent 草稿进行改进。" }],
];

export function pageIntentForPath(pathname: string): PageIntent | undefined {
  const dynamic = DYNAMIC_PAGE_INTENTS.find(([pattern]) => pattern.test(pathname));
  if (dynamic) return dynamic[1];
  const item = NAV.find((candidate) => new RegExp(`^${candidate.path.replace(":ws", "[^/]+")}$`).test(pathname));
  return item ? TOP_LEVEL_PAGE_INTENTS[item.key] : undefined;
}
