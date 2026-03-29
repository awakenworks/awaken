import { useChat } from "@ai-sdk/react";
import type { UIMessage } from "@ai-sdk/react";
import { useState, useEffect, useRef, useMemo, useCallback } from "react";
import { fetchHistory } from "@/lib/api-client";
import { parseInferenceMetrics, type InferenceMetrics } from "@/lib/protocol";
import { createTransport } from "@/lib/transport";

export type ToolCallProgressStatus =
  | "pending"
  | "running"
  | "done"
  | "failed"
  | "cancelled";

export interface ToolCallProgressNode {
  type: "tool-call-progress";
  schema: string;
  node_id: string;
  parent_node_id?: string;
  parent_call_id?: string;
  call_id?: string;
  tool_name?: string;
  status: ToolCallProgressStatus;
  progress?: number;
  loaded?: number;
  total?: number;
  message?: string;
  run_id?: string;
  parent_run_id?: string;
  thread_id?: string;
  updated_at_ms?: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function asNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function asString(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function asStatus(value: unknown): ToolCallProgressStatus {
  switch (value) {
    case "pending":
    case "running":
    case "done":
    case "failed":
    case "cancelled":
      return value;
    default:
      return "running";
  }
}

/** States that indicate a tool invocation has completed (backend or frontend). */
const TOOL_TERMINAL_STATES = new Set([
  "output-available",
  "output-error",
  "output-denied",
]);

function isToolPart(
  part: UIMessage["parts"][number],
): part is UIMessage["parts"][number] & { state: string } {
  if (!part || typeof part !== "object" || !("type" in part)) return false;
  const t = (part as { type: string }).type;
  return t === "dynamic-tool" || t.startsWith("tool-");
}

/**
 * Return true when the last assistant message contains tool parts and ALL of
 * them have reached a terminal state (output-available / output-error /
 * output-denied).  This covers both backend-executed tools and frontend tools
 * that received output via `addToolOutput`.
 */
function allToolPartsComplete(messages: UIMessage[]): boolean {
  const lastAssistant = [...messages].reverse().find((m) => m.role === "assistant");
  if (!lastAssistant) return false;
  const toolParts = lastAssistant.parts.filter(isToolPart);
  if (toolParts.length === 0) return false;
  return toolParts.every((p) => TOOL_TERMINAL_STATES.has(p.state));
}

function parseToolCallProgressSnapshot(data: unknown): ToolCallProgressNode | null {
  if (!isRecord(data)) return null;
  const activityType = asString(data.activityType);
  if (activityType !== "tool-call-progress" && activityType !== "progress") {
    return null;
  }
  const content = isRecord(data.content) ? data.content : null;
  if (!content) return null;

  const messageId = asString(data.messageId);
  const node_id = asString(content.node_id) ?? messageId;
  if (!node_id) return null;

  return {
    type: "tool-call-progress",
    schema: asString(content.schema) ?? "tool-call-progress.v1",
    node_id,
    parent_node_id: asString(content.parent_node_id),
    parent_call_id: asString(content.parent_call_id),
    call_id: asString(content.call_id),
    tool_name: asString(content.tool_name),
    status: asStatus(content.status),
    progress: asNumber(content.progress),
    loaded: asNumber(content.loaded),
    total: asNumber(content.total),
    message: asString(content.message),
    run_id: asString(content.run_id),
    parent_run_id: asString(content.parent_run_id),
    thread_id: asString(content.thread_id),
    updated_at_ms: asNumber(content.updated_at_ms),
  };
}

export interface GenerativeUISnapshot {
  activityType: string;
  messageId?: string;
  content: unknown;
}

export function useChatSession(
  threadId: string,
  agentId = "default",
  onInferenceComplete?: () => void,
) {
  const [historyLoaded, setHistoryLoaded] = useState(false);
  const [metrics, setMetrics] = useState<InferenceMetrics[]>([]);
  const [toolProgress, setToolProgress] = useState<Record<string, ToolCallProgressNode>>({});
  const [generativeUI, setGenerativeUI] = useState<Record<string, GenerativeUISnapshot>>({});
  const [askAnswers, setAskAnswers] = useState<Record<string, string>>({});
  const historyLoadToken = useRef(0);

  const transport = useMemo(
    () => createTransport(threadId, agentId),
    [threadId, agentId],
  );

  const onData = useCallback((dataPart: { type: string; data: unknown }) => {
    if (dataPart.type === "data-inference-complete") {
      const metrics = parseInferenceMetrics(dataPart.data);
      if (metrics) {
        setMetrics((prev) => [...prev, metrics]);
      }
      onInferenceComplete?.();
      return;
    }
    if (dataPart.type === "data-activity-snapshot") {
      const node = parseToolCallProgressSnapshot(dataPart.data);
      if (node) {
        setToolProgress((prev) => ({
          ...prev,
          [node.node_id]: node,
        }));
        return;
      }
      // Generative UI activity snapshots
      if (isRecord(dataPart.data)) {
        const activityType = asString(dataPart.data.activityType);
        if (activityType?.startsWith("generative-ui.")) {
          const messageId = asString(dataPart.data.messageId) ?? activityType;
          setGenerativeUI((prev) => ({
            ...prev,
            [messageId]: {
              activityType,
              messageId,
              content: dataPart.data,
            } as GenerativeUISnapshot,
          }));
        }
      }
    }
  }, [onInferenceComplete]);

  const chat = useChat({
    id: threadId,
    transport,
    onData: onData as never,
    sendAutomaticallyWhen: ({ messages }) => allToolPartsComplete(messages),
  });
  const { setMessages, messages: rawMessages } = chat;

  // Deduplicate messages by id. The AI SDK's shared global store (keyed by
  // `id: threadId`) can accumulate duplicates when StrictMode double-invokes
  // effects or when `setMessages` from history loading overlaps with streamed
  // messages that share the same id.
  const messages = useMemo(() => {
    const seen = new Set<string>();
    return rawMessages.filter((m) => {
      if (seen.has(m.id)) return false;
      seen.add(m.id);
      return true;
    });
  }, [rawMessages]);

  useEffect(() => {
    let cancelled = false;
    const token = ++historyLoadToken.current;

    setHistoryLoaded(false);
    setMetrics([]);
    setToolProgress({});
    setGenerativeUI({});
    setAskAnswers({});
    setMessages([]);

    if (!threadId) {
      setHistoryLoaded(true);
      return () => {
        cancelled = true;
      };
    }

    fetchHistory(threadId)
      .then((messages) => {
        if (cancelled || token !== historyLoadToken.current) return;
        setMessages(messages);
        setHistoryLoaded(true);
      })
      .catch(() => {
        if (cancelled || token !== historyLoadToken.current) return;
        setHistoryLoaded(true);
      });

    return () => {
      cancelled = true;
    };
  }, [threadId, setMessages]);

  return {
    ...chat,
    messages,
    historyLoaded,
    metrics,
    toolProgress,
    generativeUI,
    askAnswers,
    setAskAnswers,
  };
}
