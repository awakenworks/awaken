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
import {
  extractSurfaceId,
  normalizeA2uiMessages,
  type A2uiMessage,
} from "@/components/tools/a2ui-protocol";
import { JsonRenderPanel, OpenUIPanel, A2UIPanel } from "@/components/tools/generative-ui-renderers";
import React, { useMemo } from "react";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

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
}: MessageListProps) {
  const isDark = themeMode === "dark";
  const itemClass = isDark ? "border-slate-700" : "border-slate-100";
  const roleClass = isDark ? "text-slate-300" : "text-slate-600";
  const loadingClass = isDark ? "text-slate-400" : "text-slate-400";
  const isConversation = layout === "conversation";

  // Build a registry that merges all render_a2ui tool calls by surfaceId.
  // Only the last tool call for each surfaceId renders the actual surface.
  // Reads from tool INPUT (the A2UI message the LLM sent), not output.
  const a2uiRegistry = useMemo(() => {
    const registry = new Map<string, { allMessages: A2uiMessage[]; lastToolCallId: string }>();
    for (const m of messages) {
      for (const p of m.parts) {
        const tp = p as { type: string; toolName?: string; toolCallId?: string; input?: unknown };
        const name = tp.toolName ?? (typeof tp.type === "string" ? tp.type.replace("tool-", "") : "");
        if (name !== "render_a2ui" || tp.input == null) continue;
        const a2uiMessages = normalizeA2uiMessages(tp.input);
        const sid = extractSurfaceId(a2uiMessages);
        if (!sid || !tp.toolCallId) continue;
        const existing = registry.get(sid);
        if (existing) {
          existing.allMessages = [...existing.allMessages, ...a2uiMessages];
          existing.lastToolCallId = tp.toolCallId;
        } else {
          registry.set(sid, { allMessages: a2uiMessages, lastToolCallId: tp.toolCallId });
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
  liveGenerativeUiMessageIds,
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

function ColorPickerForm({
  toolCallId,
  colorOptions,
  onSubmit,
  themeMode,
}: {
  toolCallId: string;
  colorOptions: string[];
  onSubmit: (toolCallId: string, toolName: string, output: Record<string, unknown>) => void;
  themeMode: "light" | "dark";
}) {
  const [selected, setSelected] = React.useState<string | null>(null);
  const cardClass =
    themeMode === "dark"
      ? "mt-2 rounded-lg border border-slate-700 bg-slate-900 p-4"
      : "mt-2 rounded-lg border border-slate-200 bg-white p-4";
  const labelClass =
    themeMode === "dark" ? "text-sm text-slate-200" : "text-sm text-slate-700";

  return (
    <div data-testid="set-background-color-dialog" className={cardClass}>
      <div className={`mb-3 font-medium ${labelClass}`}>
        🎨 Choose a background color
      </div>
      <div className="flex flex-col gap-2">
        {colorOptions.map((color, index) => {
          const isSelected = selected === color;
          return (
            <label
              key={`${color}-${index}`}
              data-testid={`set-background-color-option-${index}`}
              className={`flex cursor-pointer items-center gap-3 rounded-lg border px-3 py-2 transition ${
                isSelected
                  ? "border-blue-500 bg-blue-50 dark:border-blue-400 dark:bg-blue-950"
                  : "border-slate-200 hover:border-slate-300 dark:border-slate-700 dark:hover:border-slate-600"
              }`}
            >
              <input
                type="radio"
                name={`color-${toolCallId}`}
                value={color}
                checked={isSelected}
                onChange={() => setSelected(color)}
                className="h-4 w-4 accent-blue-500"
              />
              <span
                className="h-5 w-5 rounded border border-slate-400/40"
                style={{ backgroundColor: color }}
              />
              <span className="font-mono text-xs text-slate-600 dark:text-slate-300">
                {color}
              </span>
            </label>
          );
        })}
      </div>
      <button
        type="button"
        disabled={!selected}
        onClick={() => {
          if (selected) {
            onSubmit(toolCallId, "set_background_color", {
              selected_color: selected,
            });
          }
        }}
        className={`mt-3 rounded-md px-4 py-1.5 text-sm font-medium text-white transition ${
          selected
            ? "bg-blue-600 hover:bg-blue-700"
            : "cursor-not-allowed bg-slate-300 dark:bg-slate-700"
        }`}
      >
        Apply
      </button>
    </div>
  );
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
    preliminary?: boolean;
    errorText?: string;
    approval?: { id: string; approved?: boolean; reason?: string };
  };

  const name = tool.toolName ?? tool.type.replace("tool-", "");

  // Weather card for get_weather tool
  if (name === "get_weather" && tool.output != null) {
    const output = tool.output as { location?: string };
    return <WeatherCard location={output.location ?? ""} />;
  }

  // A2UI surface renderer for render_a2ui tool — reads from INPUT (the A2UI
  // message the LLM sent), not output.  The tool output is just a confirmation.
  if (name === "render_a2ui" && tool.input != null) {
    const inputMessages = normalizeA2uiMessages(tool.input);
    const sid = extractSurfaceId(inputMessages);
    if (sid) {
      const entry = a2uiRegistry ? a2uiRegistry.get(sid) : null;
      const isLastForSurface = !entry || entry.lastToolCallId === tool.toolCallId;
      const surfaceMessages = entry ? entry.allMessages : inputMessages;
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
              <A2UIPanel
                messages={surfaceMessages}
                onAction={(text) => {
                  onSendMessage?.(text);
                }}
              />
            </>
          )}
        </div>
      );
    }
  }

  if (name === "render_json_ui" && tool.output != null) {
    const output = isRecord(tool.output) ? tool.output : null;
    const content = output?.content ?? tool.output;
    return (
      <div>
        <ToolCard
          name={name}
          state={tool.state}
          input={tool.input}
          output={tool.output}
          errorText={tool.errorText}
        />
        <JsonRenderPanel
          data={content}
          isStreaming={isLoading || tool.preliminary === true}
        />
      </div>
    );
  }

  if (name === "render_openui_ui" && tool.output != null) {
    const output = isRecord(tool.output) ? tool.output : null;
    const content = output?.content;
    const response =
      typeof content === "string"
        ? content
        : typeof tool.output === "string"
          ? tool.output
          : "";
    return (
      <div>
        <ToolCard
          name={name}
          state={tool.state}
          input={tool.input}
          output={tool.output}
          errorText={tool.errorText}
        />
        <OpenUIPanel
          response={response}
          isStreaming={isLoading || tool.preliminary === true}
        />
      </div>
    );
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
  const requestedColors = (() => {
    if (name !== "set_background_color" || tool.input == null || typeof tool.input !== "object") return [];
    const inp = tool.input as Record<string, unknown>;
    const raw = inp.colors ?? inp.palette ?? [];
    return Array.isArray(raw)
      ? raw.filter((v): v is string => typeof v === "string" && v.length > 0)
      : [];
  })();
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
        <ColorPickerForm
          toolCallId={tool.toolCallId}
          colorOptions={colorOptions}
          onSubmit={onFrontendToolSubmit}
          themeMode={themeMode}
        />
      )}
      {tool.state === "output-available" && name === "set_background_color" && tool.output != null && (
        <div
          data-testid="set-background-color-result"
          className={
            themeMode === "dark"
              ? "mt-2 flex items-center gap-2 rounded-lg border border-slate-700 bg-slate-900 px-3 py-2 text-sm text-slate-200"
              : "mt-2 flex items-center gap-2 rounded-lg border border-slate-200 bg-white px-3 py-2 text-sm text-slate-700"
          }
        >
          <span
            className="h-4 w-4 rounded-full border border-slate-400/40"
            style={{ backgroundColor: (tool.output as Record<string, unknown>)?.selected_color as string ?? "" }}
          />
          Background set to{" "}
          <code className="rounded bg-slate-100 px-1 text-xs dark:bg-slate-800">
            {(tool.output as Record<string, unknown>)?.selected_color as string ?? "unknown"}
          </code>
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
