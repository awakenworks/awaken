import type { AgentConfigItem } from "./api/types";
import { isAttachedAuxiliary } from "./agent-collaboration";

/** Every person-authored Agent, including parent-owned auxiliary Agents. */
export function authoredAgents(agents: AgentConfigItem[] | undefined): AgentConfigItem[] {
  return (agents ?? []).filter((agent) => agent.metadata?.["awaken.internal"] !== "model-test");
}

/** Runtime recovery must retain internal publications, but Console authoring and
 * selectors should only expose Agents a person intentionally created. */
export function visibleAgents(agents: AgentConfigItem[] | undefined): AgentConfigItem[] {
  return authoredAgents(agents).filter((agent) => !isAttachedAuxiliary(agent));
}
