import { describe, expect, it } from "vitest";
import { successfulAssistantDraftIds, type AssistantDraftEvent } from "./assistant-drafts";

describe("successfulAssistantDraftIds", () => {
  const call: AssistantDraftEvent = {
    id: "call-1", type: "agent.tool_use", name: "admin_draft_agent", input: { id: "reviewer" },
  };

  it("does not project pending, explicit-error, or unknown-tool calls as drafts", () => {
    expect(successfulAssistantDraftIds([call])).toEqual([]);
    expect(successfulAssistantDraftIds([call, {
      id: "result-1", type: "agent.tool_result", tool_use_id: "call-1", is_error: true,
    }])).toEqual([]);
    expect(successfulAssistantDraftIds([call, {
      id: "result-2", type: "agent.tool_result", tool_use_id: "call-1",
      content: [{ type: "text", text: "unknown tool: admin_draft_agent" }],
    }])).toEqual([]);
  });

  it("projects the exact id only after its successful paired result", () => {
    expect(successfulAssistantDraftIds([call, {
      id: "result-3", type: "agent.tool_result", tool_use_id: "call-1",
      content: [{ type: "text", text: '{"id":"reviewer","status":"draft"}' }],
    }])).toEqual(["reviewer"]);
  });
});
