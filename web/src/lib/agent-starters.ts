// Starter templates. These were private to the Quickstart step; the landing
// surface and the step now both read them, so the catalog lives here as data
// with no React dependency. A Starter seeds a draft — it never becomes a second
// configuration authority, and every field it writes stays editable afterwards.

import type { OutcomeDraft } from "./session-outcome";

export interface StarterTemplate {
  id: string;
  title: string;
  titleZh: string;
  description: string;
  descriptionZh: string;
  name: string;
  system: string;
  firstTask: string;
  firstTaskZh: string;
  requirement: string;
  requirementZh: string;
  suggestedTools: string[];
  suggestedPlugins?: string[];
  /** A Starter whose value depends on a checked deliverable proposes one. It is
   * a suggestion for the optional first-run Outcome, never an implied one: the
   * operator still opts in before any `user.define_outcome` event is sent. */
  outcome?: {
    description: string;
    descriptionZh: string;
    rubric: string;
    rubricZh: string;
    maxIterations: number;
  };
}

export const STARTERS: StarterTemplate[] = [
  {
    id: "task-assistant",
    title: "Task assistant",
    titleZh: "任务助手",
    description: "Turn one clearly-scoped request into a checked result.",
    descriptionZh: "把一项边界清晰的请求转化为经过检查的结果。",
    name: "Task Assistant",
    system: "You are a focused task assistant. Clarify the intended outcome from available context, complete the work with the configured capabilities, verify the result, and report material limitations.",
    firstTask: "Summarize your role, list the inputs you need, and complete one small representative task.",
    firstTaskZh: "概括你的职责、列出所需输入，并完成一项小型代表任务。",
    requirement: "No Sandbox unless a selected Tool or Skill needs one",
    requirementZh: "仅在所选 Tool 或 Skill 需要时创建 Sandbox",
    suggestedTools: [],
  },
  {
    id: "repository-change",
    title: "Repository change",
    titleZh: "代码仓改动",
    description: "Inspect a repository, make a focused change, and run its checks.",
    descriptionZh: "检查代码仓、完成聚焦改动并运行验证。",
    name: "Repository Change Agent",
    system: "You are a careful coding agent. Inspect the repository, make focused changes, and verify behavior before reporting completion.",
    firstTask: "Inspect the mounted repository, identify its verification commands, and report the smallest safe first change.",
    firstTaskZh: "检查已挂载代码仓，找出验证命令，并说明最小且安全的首个改动。",
    requirement: "Requires a filesystem-capable Environment / Sandbox",
    requirementZh: "需要具备文件系统能力的 Environment / Sandbox",
    suggestedTools: ["bash", "read", "write", "glob", "grep"],
  },
  {
    id: "evidence-brief",
    title: "Evidence brief",
    titleZh: "证据简报",
    description: "Answer a decision question with traceable sources and uncertainty.",
    descriptionZh: "用可追溯来源与不确定性说明回答一个决策问题。",
    name: "Evidence Brief Agent",
    system: "You are a research agent. Gather evidence, separate observation from inference, and produce a concise sourced synthesis.",
    firstTask: "Produce a short evidence brief on the supplied question, separating verified facts, inference, and open gaps.",
    firstTaskZh: "围绕给定问题生成简短证据简报，区分已验证事实、推断与待补信息。",
    requirement: "Requires a web/search Tool, MCP server, or supplied Files",
    requirementZh: "需要 Web/Search Tool、MCP Server 或已提供 Files",
    suggestedTools: ["web_search"],
    suggestedPlugins: ["web_search"],
    outcome: {
      description: "A brief that answers the supplied question, grounded in traceable sources, and closes by naming what remains uncertain.",
      descriptionZh: "一份回答给定问题、以可追溯来源为依据、并在结尾指出仍不确定之处的简报。",
      rubric: "- The brief answers the question that was actually asked.\n- Every non-obvious claim carries a source the reader can follow.\n- Observation is kept separate from inference.\n- Conflicting sources are surfaced with a reasoned judgement, not smoothed over.\n- It closes by naming the gaps and what remains uncertain.",
      rubricZh: "- 简报回答的是实际被问到的问题。\n- 每条非显而易见的结论都附有读者可追溯的来源。\n- 观察与推断被清楚区分。\n- 来源冲突被指出并给出理由判断，而不是被抹平。\n- 结尾指出待补信息与仍不确定之处。",
      maxIterations: 3,
    },
  },
];

export function starterById(id: string | null | undefined): StarterTemplate | undefined {
  return id ? STARTERS.find((starter) => starter.id === id) : undefined;
}

/** A Starter may remove the machine-id chore from a blank draft, but it must
 * never overwrite an id the operator already chose. */
export function starterAgentId(currentId: string, starterId: string): string {
  return currentId.trim() ? currentId : starterId;
}

/** Project a Starter's proposed Outcome into the authoring draft shape, in the
 * operator's language. Returns undefined for Starters that propose none. */
export function starterOutcomeDraft(
  starter: StarterTemplate,
  t: (en: string, zh: string) => string,
): OutcomeDraft | undefined {
  const proposal = starter.outcome;
  if (!proposal) return undefined;
  return {
    description: t(proposal.description, proposal.descriptionZh),
    rubric: t(proposal.rubric, proposal.rubricZh),
    maxIterations: proposal.maxIterations,
  };
}
