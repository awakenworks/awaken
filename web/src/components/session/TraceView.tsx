// The run's trace: the same committed event log read as ordered spans
// (inference → tool → inference) with inter-event durations and a JSON drill-down
// — the "see exactly why" view. The owning surface passes the same committed
// projection used by its controls and transcript.

import { useEffect } from "react";
import { Link } from "react-router";
import { EventItem, EventList, StatCard, StatGrid } from "@awaken/ui";
import { JsonInspector, Pill } from "../ui";
import { useApp } from "../../lib/app-state";
import type { SessionEvent } from "../../lib/api/types";
import type { SpanKind } from "../../lib/session-log";
import { toolDiagnostics, traceSpans } from "../../lib/session-log";

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
  log,
  loadError,
  selectedEventId,
}: {
  log: SessionEvent[];
  loadError: Error | null;
  selectedEventId?: string;
}) {
  const app = useApp();
  const spans = traceSpans(log);
  const tools = toolDiagnostics(log);

  useEffect(() => {
    if (!selectedEventId) return;
    document.getElementById(`event-${selectedEventId}`)?.scrollIntoView({ block: "center" });
  }, [selectedEventId, spans.length]);

  if (loadError) return <div className="err">{loadError.message}</div>;
  if (spans.length === 0)
    return <span className="mut">{app.t("No spans yet.", "暂无 span。")}</span>;

  return (
    <div className="trace" style={{ display: "flex", flexDirection: "column", gap: 4 }}>
      {tools.length > 0 && (
        <section className="trace-tool-summary" aria-label={app.t("Tool diagnostics", "工具诊断")}>
          <div className="trace-tool-summary-heading">
            <strong>{app.t("Tool diagnostics", "工具诊断")}</strong>
            <span className="hint">{app.t("Derived from committed Session events", "根据已提交的 Session 事件计算")}</span>
          </div>
          <StatGrid>
            {tools.map((tool) => (
              <StatCard
                key={tool.name}
                variant="metric"
                tone={tool.failures > 0 ? "danger" : tool.calls > tool.completed ? "warning" : "neutral"}
                value={tool.calls}
                label={<code>{tool.name}</code>}
                hint={app.t(
                  `${tool.failures} failed · ${tool.calls - tool.completed} pending · ${tool.medianDurationMs == null ? "—" : fmtMs(tool.medianDurationMs)} median`,
                  `${tool.failures} 次失败 · ${tool.calls - tool.completed} 次未完成 · 中位耗时 ${tool.medianDurationMs == null ? "—" : fmtMs(tool.medianDurationMs)}`,
                )}
              />
            ))}
          </StatGrid>
        </section>
      )}
      <EventList density="compact">
        {spans.map((s) => (
          <EventItem
            key={s.id}
            id={`event-${s.id}`}
            className={`trace-event${selectedEventId === s.id ? " trace-event-selected" : ""}`}
            marker={<Pill tone={s.error ? "danger" : KIND_TONE[s.kind]}>{s.kind}</Pill>}
            title={s.label}
            timestamp={s.durationMs == null ? undefined : <span className="mono">{fmtMs(s.durationMs)}</span>}
            actions={(
              <Link
                className="trace-event-link"
                to={`?view=trace&event=${encodeURIComponent(s.id)}`}
                aria-label={app.t(`Link to event ${s.id}`, `链接到事件 ${s.id}`)}
                title={app.t("Link to this event", "链接到此事件")}
              >#</Link>
            )}
          >
            {s.detail != null && <JsonInspector value={s.detail} collapsed />}
          </EventItem>
        ))}
      </EventList>
    </div>
  );
}
