import type { UIMessage } from "@ai-sdk/react";
import { DefaultChatTransport } from "ai";
import { chatApiUrl } from "./api-client";

/** Tool-part states that carry an interaction response for the backend. */
const INTERACTION_STATES = new Set([
  "approval-responded",
  "output-available",
  "output-denied",
  "output-error",
]);

/**
 * Return true when an assistant message contains at least one tool part whose
 * state represents an interaction response (approval, denial, tool output, …).
 */
function hasInteractionParts(message: UIMessage): boolean {
  return message.parts.some((part) => {
    const state =
      "state" in part && typeof part.state === "string"
        ? part.state
        : undefined;
    return state !== undefined && INTERACTION_STATES.has(state);
  });
}

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

      // Collect assistant messages that carry interaction responses (approve/deny/output).
      // The backend's extract_interaction_responses() scans assistant parts to
      // produce ToolCallDecision values, so these must be included in the body.
      const interactionMessages = messages.filter(
        (m) => m.role === "assistant" && hasInteractionParts(m),
      );

      const bodyMessages =
        trigger === "regenerate-message"
          ? (() => {
              // Backend requires at least one message; resend last user message
              const lastUser = [...messages]
                .reverse()
                .find((m) => m.role === "user");
              return lastUser ? [lastUser] : [];
            })()
          : [
              ...interactionMessages,
              ...(newUserMessages.length > 0
                ? newUserMessages
                : lastUserMsg
                  ? [lastUserMsg]
                  : []),
            ];

      return {
        body: {
          threadId: sessionId,
          runId: crypto.randomUUID(),
          messages: bodyMessages,
          ...(trigger ? { trigger } : {}),
          ...(messageId ? { messageId } : {}),
        },
      };
    },
  });
}
