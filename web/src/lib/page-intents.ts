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
    description: "Use the readiness checks and four-step journey to move from a connected model to evidence from a real Session.",
    descriptionZh: "按照就绪检查和四步路径，从连接模型开始，直到获得一次真实会话的运行证据。",
    outcome: "Start with the first readiness item that needs attention.",
    outcomeZh: "从第一个“需要处理”的就绪项开始。",
  },
  agents: {
    title: "Agents",
    titleZh: "Agent",
    description: "Define an Agent's instructions, capabilities, knowledge, and runtime behavior. Draft changes affect new Sessions only after you publish them.",
    descriptionZh: "定义 Agent 的指令、能力、知识和运行方式。草稿只有发布后才会用于新会话。",
    outcome: "Create or open an Agent, then complete Quickstart before advanced configuration.",
    outcomeZh: "新建或打开 Agent，先完成快速开始，再处理高级配置。",
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
    description: "Run real work with a published Agent and inspect the durable conversation, inputs, outputs, integrations, and trace.",
    descriptionZh: "使用已发布的 Agent 执行真实任务，并检查持久化对话、输入、输出、集成和追踪。",
    outcome: "Create a Session after its Agent and model show as ready.",
    outcomeZh: "确认 Agent 和模型已就绪后，再创建会话。",
  },
  deployments: {
    title: "Deployments",
    titleZh: "部署",
    description: "Run a published Agent on a schedule. Every scheduled or manual trigger creates a Session that you can inspect independently.",
    descriptionZh: "按计划运行已发布的 Agent。每次计划或手动触发都会创建一个可独立检查的会话。",
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
    description: "Control where Sessions run and which packages, environment variables, network access, resource limits, and Sandbox timing they receive.",
    descriptionZh: "控制会话在哪里运行，以及可用的软件包、环境变量、网络、资源限制和 Sandbox 创建时机。",
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
  a2a: {
    title: "A2A federation",
    titleZh: "A2A 联邦",
    description: "Inspect remote agent cards and discover the endpoints that let other A2A agents call this Awaken deployment.",
    descriptionZh: "检查远程 Agent Card，并了解其他 A2A Agent 调用当前 Awaken 部署的入口。",
    outcome: "Enter a configured delegate Agent ID to verify its published card.",
    outcomeZh: "输入已配置的委托 Agent ID，验证其已发布的 Agent Card。",
  },
  access: {
    title: "Access",
    titleZh: "访问控制",
    description: "Create and revoke Workspace-scoped service API keys for trusted backends. A new key is shown only once and must never be placed in browser code.",
    descriptionZh: "为可信后端创建或吊销工作区范围的 Service API Key。新 Key 只显示一次，禁止放入浏览器代码。",
    outcome: "Use the least-privileged role and copy a new key before leaving the page.",
    outcomeZh: "选择最小权限角色，并在离开页面前复制新 Key。",
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
    description: "Review this Workspace's configuration scope and jump to the pages that own providers, credentials, Environments, and runtime secrets.",
    descriptionZh: "确认当前工作区的配置范围，并前往负责供应商、凭证、运行环境和运行时凭证的页面。",
    outcome: "Use these links as configuration shortcuts; the settings themselves remain on their owning pages.",
    outcomeZh: "这里提供配置入口；具体设置仍由各自页面负责。",
  },
};

const DYNAMIC_PAGE_INTENTS: Array<[RegExp, PageIntent]> = [
  [/^\/w\/[^/]+\/agents\/new$/, {
    title: "Create Agent", titleZh: "创建 Agent",
    description: "Start with a template, choose a ready model and Environment, then complete one real run before adding optional capabilities.",
    descriptionZh: "从模板开始，选择已就绪的模型和运行环境，并先完成一次真实运行，再添加可选能力。",
    outcome: "Finish Quickstart, then review Build and publish the draft.", outcomeZh: "先完成快速开始，再检查“构建”并发布草稿。",
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
