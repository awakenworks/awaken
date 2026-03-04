import { DefaultChatTransport } from "ai";
import { chatApiUrl } from "./api-client";

export function createTransport(
  sessionId: string,
  agentId: string,
): DefaultChatTransport {
  return new DefaultChatTransport({
    api: chatApiUrl(agentId),
    headers: { "x-session-id": sessionId },
    prepareSendMessagesRequest: ({ messages, trigger, messageId }) => {
      const lastAssistantIndex = (() => {
        for (let i = messages.length - 1; i >= 0; i -= 1) {
          if (messages[i]?.role === "assistant") return i;
        }
        return -1;
      })();
      const newUserMessages = messages
        .slice(lastAssistantIndex + 1)
        .filter((m) => m.role === "user");
      const lastUserMsg =
        newUserMessages.length > 0
          ? null
          : [...messages].reverse().find((m) => m.role === "user");
      return {
        body: {
          id: sessionId,
          runId: crypto.randomUUID(),
          messages:
            trigger === "regenerate-message"
              ? []
              : newUserMessages.length > 0
                ? newUserMessages
                : lastUserMsg
                  ? [lastUserMsg]
                  : [],
          ...(trigger ? { trigger } : {}),
          ...(messageId ? { messageId } : {}),
        },
      };
    },
  });
}
