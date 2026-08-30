import { NAV, titleForPath } from "./navigation/paths";

export interface AssistantSurfaceContext {
  label: string;
  labelZh: string;
  topic: string;
  path: string;
  suggestions: string[];
  suggestionsZh: string[];
}

type SuggestionPair = readonly [en: string, zh: string];

const DEFAULT_SUGGESTIONS: readonly SuggestionPair[] = [
  ["What should I do next in this Workspace?", "这个工作区下一步应该做什么？"],
  ["Explain how Agents, Sessions, Environments, and models fit together.", "说明 Agent、会话、运行环境和模型如何协同。"],
  ["Help me create an Agent from my goal.", "根据我的目标帮我创建一个 Agent。"],
];

const SURFACES: Array<{
  pattern: RegExp;
  topic: string;
  suggestions: readonly SuggestionPair[];
}> = [
  { pattern: /\/agents\/new$/, topic: "author-agent", suggestions: [["Help me choose the right starting template.", "帮我选择合适的起始模板。"], ["What must be ready before my first real run?", "第一次真实运行前必须准备什么？"], ["Draft this Agent from my requirements.", "根据我的需求起草这个 Agent。"]] },
  { pattern: /\/agents\/[^/]+$/, topic: "agent", suggestions: [["Explain this Agent's current configuration.", "说明这个 Agent 当前的配置。"], ["Help me improve this Agent safely.", "帮我安全地改进这个 Agent。"], ["Why can't I publish or run this Agent?", "为什么这个 Agent 无法发布或运行？"]] },
  { pattern: /\/agents$/, topic: "agent", suggestions: [["Create an Agent from my goal.", "根据我的目标创建一个 Agent。"], ["Explain primary and auxiliary Agents.", "说明主 Agent 与辅助 Agent 的区别。"], ["Which Agent should I open next?", "接下来应该打开哪个 Agent？"]] },
  { pattern: /\/skills$/, topic: "skills", suggestions: [["When should I create a Skill?", "什么时候应该创建技能？"], ["Does this Skill need a Sandbox?", "这个技能是否需要 Sandbox？"], ["How do I attach a Skill to an Agent?", "如何把技能绑定到 Agent？"]] },
  { pattern: /\/files$/, topic: "files-artifacts", suggestions: [["How do I attach a file to an Agent?", "如何把文件绑定到 Agent？"], ["How are folders represented?", "文件夹如何表示？"], ["What is the difference between Files and Artifacts?", "文件与产物有什么区别？"]] },
  { pattern: /\/artifacts$/, topic: "files-artifacts", suggestions: [["Where did this Artifact come from?", "这个产物来自哪里？"], ["How do I inspect its producing Session?", "如何检查生成它的会话？"], ["Can an Artifact become a reusable input?", "产物能否作为可复用输入？"]] },
  { pattern: /\/memory\/dreams\//, topic: "memory-dreams", suggestions: [["Explain this Dream result.", "说明这个 Dream 的结果。"], ["What should I review before using it?", "使用前应该检查什么？"], ["How do Dreams differ from Memory Stores?", "Dream 与记忆库有什么区别？"]] },
  { pattern: /\/memory$/, topic: "memory-dreams", suggestions: [["How do I create and edit a Memory Store?", "如何创建和编辑记忆库？"], ["When should I run a Dream?", "什么时候应该运行 Dream？"], ["How do I bind memory to an Agent?", "如何把记忆绑定到 Agent？"]] },
  { pattern: /\/sessions\/[^/]+$/, topic: "inspect-run", suggestions: [["Help me diagnose this Session.", "帮我诊断这个会话。"], ["Explain Child runs and Trace.", "说明子运行和追踪。"], ["Where are this Session's inputs and outputs?", "这个会话的输入和输出在哪里？"]] },
  { pattern: /\/sessions$/, topic: "sessions-deployments", suggestions: [["How do I start a real Session?", "如何启动一次真实会话？"], ["Why is an Agent unavailable here?", "为什么这里无法选择某个 Agent？"], ["When should I use a Deployment instead?", "什么时候应该改用部署？"]] },
  { pattern: /\/deployments$/, topic: "sessions-deployments", suggestions: [["Help me configure a safe schedule.", "帮我配置安全的运行计划。"], ["What does Run once verify?", "“立即运行”会验证什么？"], ["How do Deployment runs appear in Sessions?", "部署运行如何显示在会话中？"]] },
  { pattern: /\/environments$/, topic: "environments", suggestions: [["Create an Environment for my workload.", "为我的工作负载创建运行环境。"], ["Which packages and network policy do I need?", "我需要哪些软件包和网络策略？"], ["When is a Sandbox required?", "什么时候必须使用 Sandbox？"]] },
  { pattern: /\/models$/, topic: "connect-model", suggestions: [["Help me connect a model provider.", "帮我连接模型供应商。"], ["Why is this model not runnable?", "为什么这个模型不可运行？"], ["How do I test a compatible endpoint?", "如何测试兼容端点？"]] },
  { pattern: /\/mcp$/, topic: "mcp", suggestions: [["How do I add an MCP server?", "如何添加 MCP 服务器？"], ["Why is an MCP server not active in a Session?", "为什么 MCP 服务器没有在会话中生效？"], ["When should prompts become Skills?", "什么时候应该把提示词做成技能？"]] },
  { pattern: /\/protocols$/, topic: "api-access", suggestions: [["How do I call a published Agent from my app?", "如何从应用调用已发布的 Agent？"], ["Which API or protocol should I use?", "应该使用哪个 API 或协议？"], ["Where do I create an API key?", "在哪里创建 API Key？"]] },
  { pattern: /\/webhooks$/, topic: "webhooks", suggestions: [["How do I verify a Webhook signature?", "如何验证 Webhook 签名？"], ["Which lifecycle events should this endpoint receive?", "这个端点应该接收哪些生命周期事件？"], ["Why was this Webhook disabled?", "为什么这个 Webhook 被停用了？"]] },
  { pattern: /\/a2a-servers$/, topic: "a2a", suggestions: [["Explain A2A federation.", "说明 A2A 联邦。"], ["How do I verify an Agent Card?", "如何验证 Agent Card？"], ["How is A2A different from an auxiliary Agent?", "A2A 与辅助 Agent 有什么区别？"]] },
  { pattern: /\/access$/, topic: "api-access", suggestions: [["Create the least-privileged API key plan.", "制定最小权限的 API Key 方案。"], ["Which role should this service use?", "这个服务应该使用哪个角色？"], ["How do I revoke a key safely?", "如何安全地吊销 Key？"]] },
  { pattern: /\/vaults$/, topic: "runtime-secrets", suggestions: [["Which Vault type should I use?", "应该使用哪种 Vault 类型？"], ["How should I name and classify this Vault?", "这个 Vault 应该如何命名和分类？"], ["Why is a Vault unavailable in this selector?", "为什么选择器中没有某个 Vault？"]] },
  { pattern: /\/settings$/, topic: "settings", suggestions: [["Where is this setting owned?", "这个设置由哪个页面负责？"], ["Explain the Workspace configuration boundaries.", "说明工作区的配置边界。"], ["What should I configure next?", "接下来应该配置什么？"]] },
  { pattern: /\/overview$/, topic: "overview", suggestions: DEFAULT_SUGGESTIONS },
  { pattern: /\/assistant$/, topic: "assistant", suggestions: DEFAULT_SUGGESTIONS },
];

function localizedTitle(pathname: string): string {
  if (/^\/w\/[^/]+\/sessions\/.+/.test(pathname)) return "会话";
  if (/^\/w\/[^/]+\/agents\/.+/.test(pathname)) return "Agent";
  if (/^\/w\/[^/]+\/memory\/dreams\/.+/.test(pathname)) return "Dream";
  if (/^\/w\/[^/]+\/assistant$/.test(pathname)) return "助手";
  const item = NAV.find((candidate) => new RegExp(`^${candidate.path.replace(":ws", "[^/]+")}$`).test(pathname));
  return item?.labelZh ?? "Console";
}

export function assistantContextForLocation(pathname: string, search = ""): AssistantSurfaceContext {
  const surface = SURFACES.find((candidate) => candidate.pattern.test(pathname));
  const title = titleForPath(pathname).title || "Console";
  const suggestions = surface?.suggestions ?? DEFAULT_SUGGESTIONS;
  return {
    label: title,
    labelZh: localizedTitle(pathname),
    topic: surface?.topic ?? "overview",
    path: `${pathname}${search}`,
    suggestions: suggestions.map(([en]) => en),
    suggestionsZh: suggestions.map(([, zh]) => zh),
  };
}
