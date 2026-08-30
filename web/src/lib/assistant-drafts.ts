const DRAFT_TOOLS = new Set(["admin_draft_agent", "admin_patch_agent"]);

export interface AssistantDraftEvent {
  id: string;
  type: string;
  name?: string;
  input?: { id?: string };
  tool_use_id?: string;
  is_error?: boolean;
  content?: Array<{ type?: string; text?: string }>;
}

function resultText(event: AssistantDraftEvent): string {
  return (event.content ?? [])
    .map((block) => typeof block.text === "string" ? block.text : "")
    .join("\n");
}

/** A draft card is a projection of a completed successful mutation, never of
 * model intent alone. Requiring the paired result prevents failed/unknown admin
 * tool calls from advertising a draft that does not exist. */
export function successfulAssistantDraftIds(events: AssistantDraftEvent[]): string[] {
  const results = new Map(
    events
      .filter((event) => event.type === "agent.tool_result" && event.tool_use_id)
      .map((event) => [event.tool_use_id as string, event]),
  );
  return Array.from(new Set(events.flatMap((event) => {
    if (event.type !== "agent.tool_use" || !event.name || !DRAFT_TOOLS.has(event.name)) return [];
    const result = results.get(event.id);
    if (!result || result.is_error === true || /unknown tool/i.test(resultText(result))) return [];
    const id = event.input?.id?.trim();
    return id ? [id] : [];
  })));
}
