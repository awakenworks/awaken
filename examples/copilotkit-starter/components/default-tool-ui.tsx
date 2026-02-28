import { CatchAllActionRenderProps } from "@copilotkit/react-core";
import { useState } from "react";
import { getMcpUiContent, McpAppFrame } from "./mcp-app-frame";

type DefaultToolProps = CatchAllActionRenderProps & {
  themeColor: string;
};

export function DefaultToolComponent({
  name,
  args,
  status,
  result,
  themeColor,
}: DefaultToolProps) {
  const [showArgs, setShowArgs] = useState(false);
  const [showResult, setShowResult] = useState(false);
  const mcpUi = getMcpUiContent(result);

  return (
    <div
      data-testid="default-tool-card"
      className="mt-3 rounded-xl border border-slate-300 bg-white/90 p-3 text-slate-900 shadow-[0_10px_24px_rgba(15,23,42,0.08)]"
    >
      <div className="flex items-center justify-between gap-2">
        <strong className="font-bold text-slate-900">Tool: {name}</strong>
        <span
          className="rounded-full border border-slate-400 px-2 py-1 text-xs font-semibold text-slate-900"
          style={{ backgroundColor: themeColor }}
        >
          {status}
        </span>
      </div>

      <div className="mt-2 grid gap-2">
        <button
          className="w-fit rounded-lg border border-slate-300 bg-white px-3 py-1.5 text-sm font-semibold text-slate-800 transition hover:border-slate-400 hover:bg-slate-50"
          onClick={() => setShowArgs((prev) => !prev)}
        >
          {showArgs ? "Hide args" : "Show args"}
        </button>
        {showArgs && (
          <pre className="m-0 overflow-x-auto rounded-lg border border-slate-200 bg-slate-50 p-2 font-mono text-xs text-slate-700">
            {JSON.stringify(args ?? {}, null, 2)}
          </pre>
        )}
      </div>

      <div className="mt-2 grid gap-2">
        <button
          className="w-fit rounded-lg border border-slate-300 bg-white px-3 py-1.5 text-sm font-semibold text-slate-800 transition hover:border-slate-400 hover:bg-slate-50"
          onClick={() => setShowResult((prev) => !prev)}
        >
          {showResult ? "Hide result" : "Show result"}
        </button>
        {showResult && (
          <pre className="m-0 overflow-x-auto rounded-lg border border-slate-200 bg-slate-50 p-2 font-mono text-xs text-slate-700">
            {typeof result === "string" ? result : JSON.stringify(result ?? {}, null, 2)}
          </pre>
        )}
      </div>

      {mcpUi && (
        <McpAppFrame
          content={mcpUi.content}
          mimeType={mcpUi.mimeType}
          resourceUri={mcpUi.resourceUri}
        />
      )}
    </div>
  );
}
