import { describe, expect, it } from "vitest";
import { createTransport } from "./transport";

type UiMsg = {
  role: string;
  id: string;
  parts?: Array<{ type: string; text?: string }>;
};

describe("createTransport", () => {
  it("sends only the latest user message", () => {
    const transport = createTransport("thread-1", "default") as unknown as {
      prepareSendMessagesRequest?: (arg: {
        messages: UiMsg[];
        trigger?: string;
        messageId?: string;
      }) => { body: { messages: UiMsg[] } };
    };

    expect(typeof transport.prepareSendMessagesRequest).toBe("function");

    const result = transport.prepareSendMessagesRequest!({
      messages: [
        { role: "user", id: "u1", parts: [{ type: "text", text: "old" }] },
        { role: "assistant", id: "a1", parts: [{ type: "text", text: "reply" }] },
        { role: "user", id: "u2", parts: [{ type: "text", text: "new" }] },
      ],
    });

    expect(result.body.messages).toHaveLength(1);
    expect(result.body.messages[0]?.id).toBe("u2");
  });

  it("sends all new user messages after the latest assistant message", () => {
    const transport = createTransport("thread-1", "default") as unknown as {
      prepareSendMessagesRequest?: (arg: {
        messages: UiMsg[];
        trigger?: string;
        messageId?: string;
      }) => { body: { messages: UiMsg[] } };
    };

    const result = transport.prepareSendMessagesRequest!({
      messages: [
        { role: "user", id: "u1", parts: [{ type: "text", text: "request" }] },
        { role: "assistant", id: "a1", parts: [{ type: "text", text: "need approval" }] },
        { role: "user", id: "u2", parts: [{ type: "tool-approval-response" }] },
      ],
    });

    expect(result.body.messages).toHaveLength(1);
    expect(result.body.messages[0]?.id).toBe("u2");
  });
});
