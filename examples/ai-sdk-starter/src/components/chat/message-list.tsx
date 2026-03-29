import type { UIMessage } from "@ai-sdk/react";
import {
  TextPart,
  ReasoningPart,
  SourceUrlPart,
  SourceDocumentPart,
  FilePart,
} from "./message-parts";
import { ToolCard } from "@/components/tools/tool-card";
import { PermissionDialog } from "@/components/tools/permission-dialog";
import { AskUserDialog } from "@/components/tools/ask-user-dialog";
import { WeatherCard } from "@/components/tools/weather-card";
import { getMcpUiContent, McpAppFrame } from "@/components/tools/mcp-app-frame";
import { A2uiSurface, extractSurfaceId, type A2uiMessage } from "@/components/tools/a2ui-surface";
import { JsonRenderPanel, OpenUIPanel, A2UIPanel } from "@/components/tools/generative-ui-renderers";
import type { GenerativeUISnapshot } from "@/hooks/use-chat-session";
import { useMemo } from "react";

type MessageListProps = {
  messages: UIMessage[];
  isLoading: boolean;
  themeMode?: "light" | "dark";
  layout?: "default" | "conversation";
  askAnswers: Record<string, string>;
  onAskAnswerChange: (toolCallId: string, value: string) => void;
  onApprove: (id: string) => void;
  onDeny: (id: string) => void;
  onAskSubmit: (toolCallId: string, answer: string) => void;
  onFrontendToolSubmit: (
    toolCallId: string,
    toolName: string,
    output: Record<string, unknown>,
  ) => Promise<void> | void;
  onSendMessage?: (text: string) => void;
  generativeUI?: Record<string, GenerativeUISnapshot>;
};

export function MessageList({
  messages,
  isLoading,
  themeMode = "light",
  layout = "default",
  askAnswers,
  onAskAnswerChange,
  onApprove,
  onDeny,
  onAskSubmit,
  onFrontendToolSubmit,
  onSendMessage,
  generativeUI = {},
}: MessageListProps) {
  const isDark = themeMode === "dark";
  const itemClass = isDark ? "border-slate-700" : "border-slate-100";
  const roleClass = isDark ? "text-slate-300" : "text-slate-600";
  const loadingClass = isDark ? "text-slate-400" : "text-slate-400";
  const isConversation = layout === "conversation";

  // Build a registry that merges all render_a2ui tool calls by surfaceId.
  // Only the last tool call for each surfaceId renders the actual surface.
  const a2uiRegistry = useMemo(() => {
    const registry = new Map<string, { allMessages: A2uiMessage[]; lastToolCallId: string }>();
    for (const m of messages) {
      for (const p of m.parts) {
        const tp = p as { type: string; toolName?: string; toolCallId?: string; output?: unknown };
        const name = tp.toolName ?? (typeof tp.type === "string" ? tp.type.replace("tool-", "") : "");
        if (name !== "render_a2ui" || tp.output == null) continue;
        const output = tp.output as { data?: { a2ui?: unknown[] } };
        const a2uiMsgs = output.data?.a2ui;
        if (!Array.isArray(a2uiMsgs) || a2uiMsgs.length === 0) continue;
        const sid = extractSurfaceId(a2uiMsgs as A2uiMessage[]);
        if (!sid || !tp.toolCallId) continue;
        const existing = registry.get(sid);
        if (existing) {
          existing.allMessages = [...existing.allMessages, ...(a2uiMsgs as A2uiMessage[])];
          existing.lastToolCallId = tp.toolCallId;
        } else {
          registry.set(sid, { allMessages: [...(a2uiMsgs as A2uiMessage[])], lastToolCallId: tp.toolCallId });
        }
      }
    }
    return registry;
  }, [messages]);

  return (
    <div className="chat-messages flex-1 overflow-y-auto px-4 py-3">
      {messages.map((m) => (
        <div key={m.id} className={isConversation ? "" : `border-b py-2 last:border-0 ${itemClass}`}>
          {isConversation ? (
            <ConversationItem
              role={m.role}
              isDark={isDark}
              roleClass={roleClass}
              content={
                <>
                  {m.parts.map((p, i) => (
                    <PartRenderer
                      key={i}
                      part={p}
                      isLoading={isLoading}
                      askAnswers={askAnswers}
                      onAskAnswerChange={onAskAnswerChange}
                      onApprove={onApprove}
                      onDeny={onDeny}
                      onAskSubmit={onAskSubmit}
                      onFrontendToolSubmit={onFrontendToolSubmit}
                      onSendMessage={onSendMessage}
                      themeMode={themeMode}
                      a2uiRegistry={a2uiRegistry}
                    />
                  ))}
                </>
              }
            />
          ) : (
            <>
              <strong className={`text-sm ${roleClass}`}>
                {m.role === "user" ? "You" : "Agent"}:
              </strong>
              {m.parts.map((p, i) => (
                <PartRenderer
                  key={i}
                  part={p}
                  isLoading={isLoading}
                  askAnswers={askAnswers}
                  onAskAnswerChange={onAskAnswerChange}
                  onApprove={onApprove}
                  onDeny={onDeny}
                  onAskSubmit={onAskSubmit}
                  onFrontendToolSubmit={onFrontendToolSubmit}
                  themeMode={themeMode}
                  a2uiRegistry={a2uiRegistry}
                />
              ))}
            </>
          )}
        </div>
      ))}
      {/* Generative UI panels from activity snapshots */}
      {Object.values(generativeUI).map((snapshot) => (
        <GenerativeUIRenderer key={snapshot.messageId ?? snapshot.activityType} snapshot={snapshot} isStreaming={isLoading} />
      ))}
      {isLoading && (
        <div className={`py-2 text-sm ${loadingClass}`}>Thinking...</div>
      )}
    </div>
  );
}

function ConversationItem({
  role,
  isDark,
  roleClass,
  content,
}: {
  role: string;
  isDark: boolean;
  roleClass: string;
  content: JSX.Element;
}) {
  const isUser = role === "user";
  const borderClass = isDark ? "border-slate-800" : "border-slate-100";

  return (
    <div className={`border-b py-4 last:border-b-0 ${borderClass}`}>
      <div className="mx-auto max-w-3xl">
        <div className={`mb-1.5 text-xs font-semibold ${roleClass}`}>
          {isUser ? "You" : "Assistant"}
        </div>
        <div className={isUser ? "" : ""}>{content}</div>
      </div>
    </div>
  );
}

type A2uiRegistryMap = Map<string, { allMessages: A2uiMessage[]; lastToolCallId: string }>;

function PartRenderer({
  part,
  isLoading,
  askAnswers,
  onAskAnswerChange,
  onApprove,
  onDeny,
  onAskSubmit,
  onFrontendToolSubmit,
  onSendMessage,
  themeMode,
  a2uiRegistry,
}: {
  part: Record<string, unknown>;
  isLoading: boolean;
  askAnswers: Record<string, string>;
  onAskAnswerChange: (toolCallId: string, value: string) => void;
  onApprove: (id: string) => void;
  onDeny: (id: string) => void;
  onAskSubmit: (toolCallId: string, answer: string) => void;
  onFrontendToolSubmit: (
    toolCallId: string,
    toolName: string,
    output: Record<string, unknown>,
  ) => Promise<void> | void;
  onSendMessage?: (text: string) => void;
  themeMode: "light" | "dark";
  a2uiRegistry?: A2uiRegistryMap;
}) {
  const p = part as { type: string; text?: string; state?: string; url?: string; title?: string; filename?: string; mediaType?: string };
  if (p.type === "text") {
    return <TextPart text={p.text ?? ""} />;
  }
  if (p.type === "reasoning") {
    return <ReasoningPart text={p.text} state={p.state} />;
  }
  if (p.type === "source-url") {
    return <SourceUrlPart url={p.url ?? ""} title={p.title} />;
  }
  if (p.type === "source-document") {
    return <SourceDocumentPart title={p.title ?? ""} filename={p.filename} mediaType={p.mediaType} />;
  }
  if (p.type === "file") {
    return <FilePart url={p.url ?? ""} mediaType={p.mediaType} />;
  }
  if (p.type === "dynamic-tool" || p.type.startsWith("tool-")) {
    return (
      <ToolPartRenderer
        part={p}
        isLoading={isLoading}
        askAnswers={askAnswers}
        onAskAnswerChange={onAskAnswerChange}
        onApprove={onApprove}
        onDeny={onDeny}
        onAskSubmit={onAskSubmit}
        onFrontendToolSubmit={onFrontendToolSubmit}
        onSendMessage={onSendMessage}
        themeMode={themeMode}
        a2uiRegistry={a2uiRegistry}
      />
    );
  }
  return null;
}

function ToolPartRenderer({
  part,
  isLoading,
  askAnswers,
  onAskAnswerChange,
  onApprove,
  onDeny,
  onAskSubmit,
  onFrontendToolSubmit,
  onSendMessage,
  themeMode,
  a2uiRegistry,
}: {
  part: Record<string, unknown>;
  isLoading: boolean;
  askAnswers: Record<string, string>;
  onAskAnswerChange: (toolCallId: string, value: string) => void;
  onApprove: (id: string) => void;
  onDeny: (id: string) => void;
  onAskSubmit: (toolCallId: string, answer: string) => void;
  onFrontendToolSubmit: (
    toolCallId: string,
    toolName: string,
    output: Record<string, unknown>,
  ) => Promise<void> | void;
  onSendMessage?: (text: string) => void;
  themeMode: "light" | "dark";
  a2uiRegistry?: A2uiRegistryMap;
}) {
  const tool = part as {
    type: string;
    toolName?: string;
    toolCallId: string;
    state: string;
    input?: unknown;
    output?: unknown;
    errorText?: string;
    approval?: { id: string; approved?: boolean; reason?: string };
  };

  const name = tool.toolName ?? tool.type.replace("tool-", "");

  // Weather card for get_weather tool
  if (name === "get_weather" && tool.output != null) {
    const output = tool.output as { location?: string };
    return <WeatherCard location={output.location ?? ""} />;
  }

  // A2UI surface renderer for render_a2ui tool
  if (name === "render_a2ui" && tool.output != null) {
    const output = tool.output as { data?: { a2ui?: unknown[] } };
    const a2uiMessages = output.data?.a2ui;
    if (Array.isArray(a2uiMessages) && a2uiMessages.length > 0) {
      const sid = extractSurfaceId(a2uiMessages as A2uiMessage[]);
      const entry = sid && a2uiRegistry ? a2uiRegistry.get(sid) : null;
      // Only render the surface at the last tool call for this surfaceId
      const isLastForSurface = !entry || entry.lastToolCallId === tool.toolCallId;
      const surfaceMessages = entry ? entry.allMessages : (a2uiMessages as A2uiMessage[]);
      return (
        <div>
          <ToolCard
            name={name}
            state={tool.state}
            input={tool.input}
            output={tool.output}
            errorText={tool.errorText}
          />
          {isLastForSurface && (
            <>
              {/* Official @a2ui/lit Web Component renderer */}
              <A2UIPanel messages={surfaceMessages} />
              {/* Fallback pure-React renderer */}
              <A2uiSurface
                messages={surfaceMessages as never[]}
                onEvent={(surfaceId, eventName, context) => {
                  if (onSendMessage) {
                    const ctx = JSON.stringify(context);
                    onSendMessage(
                      `[A2UI event on surface "${surfaceId}": ${eventName}] ${ctx}`,
                    );
                  }
                }}
              />
            </>
          )}
        </div>
      );
    }
  }

  // MCP App iframe for tools with mcp.ui.content metadata
  const mcpUi = getMcpUiContent(tool.output);
  if (mcpUi) {
    return (
      <div>
        <ToolCard
          name={name}
          state={tool.state}
          input={tool.input}
          output={tool.output}
          errorText={tool.errorText}
        />
        <McpAppFrame
          content={mcpUi.content}
          mimeType={mcpUi.mimeType}
          resourceUri={mcpUi.resourceUri}
        />
      </div>
    );
  }

  const requestedToolName =
    name === "PermissionConfirm" &&
    tool.input != null &&
    typeof tool.input === "object" &&
    "tool_name" in (tool.input as Record<string, unknown>)
      ? String((tool.input as Record<string, unknown>).tool_name)
      : name;

  const askPrompt =
    name === "askUserQuestion" &&
    tool.input != null &&
    typeof tool.input === "object" &&
    "message" in (tool.input as Record<string, unknown>)
      ? String((tool.input as Record<string, unknown>).message)
      : "";

  const highlightCandidates =
    name === "highlight_place" && tool.input != null && typeof tool.input === "object"
      ? (((tool.input as Record<string, unknown>).candidates as unknown[]) ?? [])
          .filter((value): value is string => typeof value === "string" && value.length > 0)
      : [];
  const placeOptions =
    highlightCandidates.length > 0 ? highlightCandidates : ["Eiffel Tower", "Tokyo Station", "Golden Gate Bridge"];
  const requestedColors =
    name === "set_background_color" && tool.input != null && typeof tool.input === "object"
      ? (((tool.input as Record<string, unknown>).colors as unknown[]) ??
          ((tool.input as Record<string, unknown>).palette as unknown[]) ??
          [])
          .filter((value): value is string => typeof value === "string" && value.length > 0)
      : [];
  const colorOptions =
    requestedColors.length > 0
      ? requestedColors
      : ["#f8fafc", "#dbeafe", "#dcfce7", "#fee2e2", "#fef3c7"];

  return (
    <div>
      <ToolCard
        name={name}
        state={tool.state}
        input={tool.input}
        output={tool.output}
        errorText={tool.errorText}
      />
      {tool.state === "approval-requested" && tool.approval?.id && (
        <PermissionDialog
          requestedToolName={requestedToolName}
          approvalId={tool.approval.id}
          onApprove={onApprove}
          onDeny={onDeny}
          themeMode={themeMode}
        />
      )}
      {tool.state === "input-available" && name === "askUserQuestion" && (
        <AskUserDialog
          toolCallId={tool.toolCallId}
          prompt={askPrompt}
          value={askAnswers[tool.toolCallId] ?? ""}
          onChange={(v) => onAskAnswerChange(tool.toolCallId, v)}
          onSubmit={onAskSubmit}
          disabled={false}
          themeMode={themeMode}
        />
      )}
      {tool.state === "input-available" && name === "highlight_place" && (
        <div
          data-testid="highlight-place-dialog"
          className={
            themeMode === "dark"
              ? "mt-2 rounded-lg border border-slate-700 bg-slate-900 p-3"
              : "mt-2 rounded-lg border border-slate-200 bg-white p-3"
          }
        >
          <div
            className={
              themeMode === "dark"
                ? "mb-2 text-sm text-slate-200"
                : "mb-2 text-sm text-slate-800"
            }
          >
            Pick a place to highlight on the frontend map view:
          </div>
          <div className="flex flex-wrap gap-2">
            {placeOptions.map((place) => (
              <button
                key={place}
                type="button"
                data-testid={`highlight-place-option-${place}`}
                onClick={() =>
                  onFrontendToolSubmit(tool.toolCallId, "highlight_place", {
                    place,
                    highlighted: true,
                  })
                }
                disabled={false}
                className={
                  themeMode === "dark"
                    ? "rounded-full border border-cyan-700/50 bg-cyan-900/30 px-3 py-1.5 text-xs font-semibold text-cyan-100 transition hover:bg-cyan-800/40 disabled:opacity-50"
                    : "rounded-full border border-cyan-200 bg-cyan-50 px-3 py-1.5 text-xs font-semibold text-cyan-900 transition hover:bg-cyan-100 disabled:opacity-50"
                }
              >
                {place}
              </button>
            ))}
          </div>
        </div>
      )}
      {tool.state === "input-available" && name === "set_background_color" && (
        <div
          data-testid="set-background-color-dialog"
          className={
            themeMode === "dark"
              ? "mt-2 rounded-lg border border-slate-700 bg-slate-900 p-3"
              : "mt-2 rounded-lg border border-slate-200 bg-white p-3"
          }
        >
          <div
            className={
              themeMode === "dark"
                ? "mb-2 text-sm text-slate-200"
                : "mb-2 text-sm text-slate-800"
            }
          >
            Select a background color for this chat panel:
          </div>
          <div className="flex flex-wrap gap-2">
            {colorOptions.map((color, index) => (
              <button
                key={`${color}-${index}`}
                type="button"
                data-testid={`set-background-color-option-${index}`}
                onClick={() =>
                  onFrontendToolSubmit(tool.toolCallId, "set_background_color", {
                    color,
                  })
                }
                disabled={false}
                className={
                  themeMode === "dark"
                    ? "inline-flex items-center gap-2 rounded-full border border-slate-600 bg-slate-800 px-3 py-1.5 text-xs font-semibold text-slate-100 transition hover:bg-slate-700 disabled:opacity-50"
                    : "inline-flex items-center gap-2 rounded-full border border-slate-300 bg-white px-3 py-1.5 text-xs font-semibold text-slate-800 transition hover:bg-slate-100 disabled:opacity-50"
                }
              >
                <span
                  className="h-3 w-3 rounded-full border border-slate-400/60"
                  style={{ backgroundColor: color }}
                />
                {color}
              </button>
            ))}
          </div>
        </div>
      )}
      {tool.state === "output-denied" && (
        <div
          data-testid="permission-denied"
          className="mt-1 text-sm text-red-700"
        >
          Permission denied
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Generative UI activity snapshot renderer
// ---------------------------------------------------------------------------

function GenerativeUIRenderer({
  snapshot,
  isStreaming,
}: {
  snapshot: GenerativeUISnapshot;
  isStreaming: boolean;
}) {
  const content = snapshot.content as Record<string, unknown> | undefined;
  if (!content) return null;

  const activityType = snapshot.activityType;

  if (activityType === "generative-ui.json-render") {
    // content.content holds the json-render Spec
    const specData = content.content ?? content;
    return <JsonRenderPanel data={specData} />;
  }

  if (activityType === "generative-ui.openui-lang") {
    // content.content holds the raw openui-lang text
    const response = typeof content.content === "string" ? content.content : "";
    return <OpenUIPanel response={response} isStreaming={isStreaming} />;
  }

  // Fallback: render raw JSON for unknown generative-ui types
  return (
    <div data-testid="generative-ui-unknown" className="my-2 rounded-lg border border-slate-200 bg-slate-50 p-2">
      <div className="text-xs font-semibold text-slate-500 mb-1">{activityType}</div>
      <pre className="text-xs text-slate-600 overflow-x-auto">
        {JSON.stringify(content, null, 2)}
      </pre>
    </div>
  );
}
