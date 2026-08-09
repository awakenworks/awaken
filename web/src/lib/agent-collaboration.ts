import type {
  AgentConfigItem,
  DelegationLimits,
  MultiagentConfig,
  MultiagentTarget,
} from "./api/types";

export const DEFAULT_DELEGATION_LIMITS: DelegationLimits = {
  max_depth: 8,
  max_parallel: 8,
  max_total: 64,
};
export const CONSERVATIVE_DELEGATION_LIMITS: DelegationLimits = {
  max_depth: 2,
  max_parallel: 2,
  max_total: 8,
};
export const AUXILIARY_PARENT_KEY = "awaken.parent_agent_id";
export const AUXILIARY_ROLE_KEY = "awaken.agent_role";

export function isAttachedAuxiliary(agent: Pick<AgentConfigItem, "metadata">): boolean {
  return Boolean(agent.metadata?.[AUXILIARY_PARENT_KEY]);
}

export interface DelegateTargetView {
  id: string;
  version?: number;
  recursiveSelf: boolean;
}

export function delegateTargetView(target: MultiagentTarget, ownerId: string): DelegateTargetView {
  if (typeof target === "string") return { id: target, recursiveSelf: false };
  if (target.type === "self") return { id: ownerId, recursiveSelf: true };
  return { id: target.id, version: target.version, recursiveSelf: false };
}

export function agentTarget(id: string, version?: number): MultiagentTarget {
  return version === undefined ? id : { type: "agent", id, version };
}

export function rosterOf(
  config: Pick<AgentConfigItem, "id" | "multiagent"> & Partial<Pick<AgentConfigItem, "metadata">>,
): DelegateTargetView[] {
  if (config.multiagent) {
    return config.multiagent.agents.map((target) => delegateTargetView(target, config.id));
  }
  return isAttachedAuxiliary(config) ? [] : [{ id: config.id, recursiveSelf: true }];
}

export function withRoster(targets: MultiagentTarget[]): MultiagentConfig | null {
  return targets.length > 0 ? { type: "coordinator", agents: targets } : null;
}

export interface CollaborationProjection {
  coordinators: AgentConfigItem[];
  referencedAgentIds: Set<string>;
  brokenReferences: Array<{ coordinatorId: string; targetId: string }>;
  recursiveCoordinatorIds: Set<string>;
  unusedAgents: AgentConfigItem[];
}

export function projectCollaborations(agents: AgentConfigItem[]): CollaborationProjection {
  const known = new Set(agents.map((agent) => agent.id));
  const referencedAgentIds = new Set<string>();
  const brokenReferences: Array<{ coordinatorId: string; targetId: string }> = [];
  const recursiveCoordinatorIds = new Set<string>();
  const coordinators = agents.filter((agent) => !isAttachedAuxiliary(agent));

  for (const coordinator of coordinators) {
    for (const target of rosterOf(coordinator)) {
      if (target.recursiveSelf) {
        recursiveCoordinatorIds.add(coordinator.id);
        continue;
      }
      referencedAgentIds.add(target.id);
      if (!known.has(target.id)) {
        brokenReferences.push({ coordinatorId: coordinator.id, targetId: target.id });
      }
    }
  }

  return {
    coordinators,
    referencedAgentIds,
    brokenReferences,
    recursiveCoordinatorIds,
    unusedAgents: agents.filter((agent) => isAttachedAuxiliary(agent) && !referencedAgentIds.has(agent.id)),
  };
}
