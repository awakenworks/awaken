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

type MessageListProps = {
  messages: UIMessage[];
  isLoading: boolean;
  askAnswers: Record<string, string>;
  onAskAnswerChange: (toolCallId: string, value: string) => void;
  onApprove: (id: string) => void;
  onDeny: (id: string) => void;
  onAskSubmit: (toolCallId: string, answer: string) => void;
};

export function MessageList({
  messages,
  isLoading,
  askAnswers,
  onAskAnswerChange,
  onApprove,
  onDeny,
  onAskSubmit,
}: MessageListProps) {
  return (
    <div className="chat-messages flex-1 overflow-y-auto px-4 py-3">
      {messages.map((m) => (
        <div
          key={m.id}
          className="border-b border-slate-100 py-2 last:border-0"
        >
          <strong className="text-sm text-slate-600">
            {m.role === "user" ? "You" : "Agent"}:
          </strong>
          {m.parts.map((p, i) => {
            if (p.type === "text") {
              return <TextPart key={i} text={p.text} />;
            }
            if (p.type === "reasoning") {
              const r = p as { text?: string; state?: string };
              return <ReasoningPart key={i} text={r.text} state={r.state} />;
            }
            if (p.type === "source-url") {
              const s = p as { url: string; title?: string };
              return <SourceUrlPart key={i} url={s.url} title={s.title} />;
            }
            if (p.type === "source-document") {
              const s = p as { title: string; filename?: string; mediaType?: string };
              return (
                <SourceDocumentPart
                  key={i}
                  title={s.title}
                  filename={s.filename}
                  mediaType={s.mediaType}
                />
              );
            }
            if (p.type === "file") {
              const f = p as { url: string; mediaType?: string };
              return <FilePart key={i} url={f.url} mediaType={f.mediaType} />;
            }
            if (p.type === "dynamic-tool" || p.type.startsWith("tool-")) {
              return (
                <ToolPartRenderer
                  key={i}
                  part={p}
                  isLoading={isLoading}
                  askAnswers={askAnswers}
                  onAskAnswerChange={onAskAnswerChange}
                  onApprove={onApprove}
                  onDeny={onDeny}
                  onAskSubmit={onAskSubmit}
                />
              );
            }
            return null;
          })}
        </div>
      ))}
      {isLoading && (
        <div className="py-2 text-sm text-slate-400">Thinking...</div>
      )}
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
}: {
  part: Record<string, unknown>;
  isLoading: boolean;
  askAnswers: Record<string, string>;
  onAskAnswerChange: (toolCallId: string, value: string) => void;
  onApprove: (id: string) => void;
  onDeny: (id: string) => void;
  onAskSubmit: (toolCallId: string, answer: string) => void;
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
        />
      )}
      {tool.state === "input-available" && name === "askUserQuestion" && (
        <AskUserDialog
          toolCallId={tool.toolCallId}
          prompt={askPrompt}
          value={askAnswers[tool.toolCallId] ?? ""}
          onChange={(v) => onAskAnswerChange(tool.toolCallId, v)}
          onSubmit={onAskSubmit}
          disabled={isLoading}
        />
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
