/// View-model layer over the @ai-sdk/react UIMessage parts.
///
/// We deliberately avoid importing the SDK's internal types — its part
/// shapes evolve between minor versions, and we only care about a small
/// projection (text + tool-call lifecycle) for rendering. Anything we
/// don't recognise becomes an "unknown" block so the user can still see
/// _that something happened_ without us crashing on a future version.

export type AssistantBlockTone = "info" | "success" | "warn" | "error";

export type AssistantBlock =
  | { kind: "text"; id: string; text: string }
  | { kind: "reasoning"; id: string; text: string }
  | { kind: "step-start"; id: string }
  | { kind: "runtime-metadata"; id: string; parts: RuntimeDataPart[] }
  | {
      kind: "tool-call";
      id: string;
      toolName: string;
      state: ToolCallState;
      input?: unknown;
      output?: unknown;
      errorText?: string;
    }
  | { kind: "unknown"; id: string; type: string };

export type RuntimeDataPartType =
  | "data-run-info"
  | "data-inference-complete"
  | "data-state-snapshot";

export interface RuntimeDataPart {
  type: RuntimeDataPartType;
  data?: unknown;
}

export type ToolCallState =
  | "input-streaming"
  | "input-available"
  | "approval-requested"
  | "approval-responded"
  | "output-available"
  | "output-error"
  | "output-denied"
  | "unknown";

export interface AssistantMessageView {
  blocks: AssistantBlock[];
}

interface RawPart {
  type?: unknown;
  text?: unknown;
  toolName?: unknown;
  toolCallId?: unknown;
  input?: unknown;
  output?: unknown;
  state?: unknown;
  errorText?: unknown;
  data?: unknown;
}

export function viewMessage(message: { parts?: unknown }): AssistantMessageView {
  const parts = Array.isArray(message.parts) ? message.parts : [];
  const blocks: AssistantBlock[] = [];
  const runtimeParts: RuntimeDataPart[] = [];

  parts.forEach((rawPart, index) => {
    const part = (rawPart ?? {}) as RawPart;
    const type = typeof part.type === "string" ? part.type : "";
    const id = `${index}`;

    if (type === "text") {
      const text = typeof part.text === "string" ? part.text : "";
      if (text.trim().length > 0) {
        blocks.push({ kind: "text", id, text });
      }
      return;
    }

    if (type === "reasoning") {
      const text = typeof part.text === "string" ? part.text : "";
      if (text.trim().length > 0) {
        blocks.push({ kind: "reasoning", id, text });
      }
      return;
    }

    if (type === "step-start") {
      blocks.push({ kind: "step-start", id });
      return;
    }

    if (type === "dynamic-tool" || type.startsWith("tool-")) {
      const toolName =
        typeof part.toolName === "string"
          ? part.toolName
          : type.startsWith("tool-")
            ? type.slice(5)
            : "(unknown)";
      blocks.push({
        kind: "tool-call",
        id,
        toolName,
        state: classifyToolState(part.state),
        input: part.input,
        output: part.output,
        errorText:
          typeof part.errorText === "string" ? part.errorText : undefined,
      });
      return;
    }

    if (isRuntimeDataPart(type)) {
      runtimeParts.push({ type, data: part.data });
      return;
    }

    blocks.push({ kind: "unknown", id, type });
  });

  if (runtimeParts.length > 0) {
    blocks.push({
      kind: "runtime-metadata",
      id: "runtime-metadata",
      parts: runtimeParts,
    });
  }

  return { blocks };
}

function isRuntimeDataPart(type: string): type is RuntimeDataPartType {
  return (
    type === "data-run-info" ||
    type === "data-inference-complete" ||
    type === "data-state-snapshot"
  );
}

function classifyToolState(value: unknown): ToolCallState {
  if (typeof value !== "string") return "unknown";
  switch (value) {
    case "input-streaming":
    case "input-available":
    case "approval-requested":
    case "approval-responded":
    case "output-available":
    case "output-error":
    case "output-denied":
      return value;
    default:
      return "unknown";
  }
}

export function describeToolCallState(state: ToolCallState): {
  label: string;
  tone: AssistantBlockTone;
} {
  switch (state) {
    case "input-streaming":
      return { label: "Preparing input…", tone: "info" };
    case "input-available":
      return { label: "Calling tool…", tone: "info" };
    case "approval-requested":
      return { label: "Waiting for approval", tone: "warn" };
    case "approval-responded":
      return { label: "Approval responded", tone: "info" };
    case "output-available":
      return { label: "Completed", tone: "success" };
    case "output-error":
      return { label: "Failed", tone: "error" };
    case "output-denied":
      return { label: "Denied", tone: "warn" };
    case "unknown":
      return { label: "Unknown state", tone: "info" };
  }
}

/// Best-effort serialiser used to render tool input/output payloads.
/// Strings stay verbatim; everything else is JSON-stringified with
/// readable indentation. Returns null for `undefined` so callers can
/// skip rendering empty payloads.
export function previewPayload(value: unknown): string | null {
  if (value === undefined) return null;
  if (value === null) return "null";
  if (typeof value === "string") {
    return value.length > 0 ? value : null;
  }
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}
