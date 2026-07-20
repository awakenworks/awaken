// Small browser event seam between the editor and the global Admin Assistant panel.
// The config plane remains the source of truth; events only coordinate UI refresh and
// automated repair without coupling either surface to the other's React tree.

export const ASSISTANT_REPAIR_EVENT = "awaken:assistant-repair";
export const AGENT_DRAFT_CHANGED_EVENT = "awaken:agent-draft-changed";
export const ASSISTANT_SETTLED_EVENT = "awaken:assistant-settled";

export interface AssistantRepairDetail {
  id: string;
  requestId: string;
  message: string;
}

