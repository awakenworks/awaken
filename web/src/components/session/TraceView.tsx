// The run's trace: the same committed event log read as ordered spans
// (inference → tool → inference) with inter-event durations and a JSON drill-down
// — the "see exactly why" view. Reuses useSessionLog, so it stays in lockstep with
// the transcript over one source of truth.

import { Pill } from "../ui";
import { JsonInspector } from "../ui";
import { useApp } from "../../lib/app-state";
import type { SpanKind } from "../../lib/session-log";
import { traceSpans } from "../../lib/session-log";
import { useSessionLog } from "../../lib/useSessionLog";

const KIND_TONE: Record<SpanKind, "agent" | "neutral" | "ok" | "warn"> = {
  inference: "agent",
  tool: "neutral",
  tool_result: "ok",
  status: "warn",
  outcome: "neutral",
  other: "neutral",
};

function fmtMs(ms?: number): string {
  if (ms == null) return "";
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(2)}s`;
}

export default function TraceView({
  base,
  queryKey,
  live = true,
}: {
  base: string;
  queryKey: readonly unknown[];
  live?: boolean;
}) {
  const app = useApp();
  const { log, loadError } = useSessionLog(base, queryKey, live);
  const spans = traceSpans(log);

  if (loadError) return <div className="err">{loadError.message}</div>;
  if (spans.length === 0)
    return <span className="mut">{app.t("No spans yet.", "暂无 span。")}</span>;

  return (
    <div className="trace" style={{ display: "flex", flexDirection: "column", gap: 4 }}>
      {spans.map((s) => (
        <div
          key={s.id}
          className="row"
          style={{ alignItems: "flex-start", gap: 8, padding: "4px 0", borderBottom: "1px solid var(--line)" }}
        >
          <Pill tone={s.error ? "danger" : KIND_TONE[s.kind]}>{s.kind}</Pill>
          <code style={{ flex: 1, minWidth: 0, wordBreak: "break-word" }}>{s.label}</code>
          {s.durationMs != null && (
            <span className="mut mono" style={{ fontSize: 11 }}>
              {fmtMs(s.durationMs)}
            </span>
          )}
          {s.detail != null && (
            <div style={{ flexBasis: "100%" }}>
              <JsonInspector value={s.detail} collapsed />
            </div>
          )}
        </div>
      ))}
    </div>
  );
}
