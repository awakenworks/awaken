import { useChat, type UIMessage } from "@ai-sdk/react";
import { useState, useEffect, useRef, useMemo, useCallback } from "react";
import { fetchHistory } from "@/lib/api-client";
import { createTransport } from "@/lib/transport";

export interface InferenceMetrics {
  model: string;
  usage?: {
    prompt_tokens?: number;
    completion_tokens?: number;
    total_tokens?: number;
  };
  duration_ms: number;
}

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

export function useChatSession(threadId: string, agentId = "default") {
  const [historyLoaded, setHistoryLoaded] = useState(false);
  const [metrics, setMetrics] = useState<InferenceMetrics[]>([]);
  const [toolProgress, setToolProgress] = useState<Record<string, ToolCallProgressNode>>({});
  const [askAnswers, setAskAnswers] = useState<Record<string, string>>({});
  const autoSubmittedIds = useRef<Set<string>>(new Set());
  const historyLoadToken = useRef(0);

  const transport = useMemo(
    () => createTransport(threadId, agentId),
    [threadId, agentId],
  );

  const onData = useCallback((dataPart: { type: string; data: unknown }) => {
    if (dataPart.type === "data-inference-complete") {
      setMetrics((prev) => [...prev, dataPart.data as InferenceMetrics]);
      return;
    }
    if (dataPart.type === "data-activity-snapshot") {
      const node = parseToolCallProgressSnapshot(dataPart.data);
      if (!node) return;
      setToolProgress((prev) => ({
        ...prev,
        [node.node_id]: node,
      }));
    }
  }, []);

  const sendAutomaticallyWhen = useCallback(
    ({ messages }: { messages: UIMessage[] }) => {
      const last = messages[messages.length - 1];
      if (!last) return false;

      const interactionIds = last.parts.flatMap((part) => {
        if (
          (part.type === "dynamic-tool" || part.type.startsWith("tool-")) &&
          "state" in part &&
          typeof part.state === "string"
        ) {
          const state = part.state;
          const toolCallId =
            "toolCallId" in part && typeof part.toolCallId === "string"
              ? part.toolCallId
              : undefined;
          const toolName =
            "toolName" in part && typeof part.toolName === "string"
              ? part.toolName
              : part.type.startsWith("tool-")
                ? part.type.slice("tool-".length)
                : undefined;

          if (state === "approval-responded") {
            const approval = (part as { approval?: { id?: unknown } }).approval;
            if (approval && typeof approval.id === "string") return [approval.id];
            if (toolCallId) return [toolCallId];
          }

          if (
            (toolName === "PermissionConfirm" ||
              toolName === "askUserQuestion" ||
              toolName === "highlight_place" ||
              toolName === "set_background_color") &&
            (state === "output-available" || state === "output-denied" || state === "output-error")
          ) {
            if (toolCallId) return [toolCallId];
          }
        }
        return [];
      });

      let shouldSend = false;
      for (const id of interactionIds) {
        if (!autoSubmittedIds.current.has(id)) {
          autoSubmittedIds.current.add(id);
          shouldSend = true;
        }
      }
      return shouldSend;
    },
    [],
  );

  const chat = useChat({
    id: threadId,
    transport,
    onData: onData as never,
    sendAutomaticallyWhen,
  });
  const { setMessages } = chat;

  useEffect(() => {
    let cancelled = false;
    const token = ++historyLoadToken.current;

    setHistoryLoaded(false);
    setMetrics([]);
    setToolProgress({});
    setAskAnswers({});
    autoSubmittedIds.current = new Set();
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
    historyLoaded,
    metrics,
    toolProgress,
    askAnswers,
    setAskAnswers,
  };
}
