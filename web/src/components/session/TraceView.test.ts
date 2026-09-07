import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it, vi } from "vitest";
import TraceView from "./TraceView";
import type { SessionEvent } from "../../lib/api/types";

vi.mock("../../lib/app-state", () => ({ useApp: () => ({ t: (en: string) => en }) }));

describe("trace event layout", () => {
  // T1 tool/error/long label -> kind belongs to accessible content, never the
  // aria-hidden narrow rail; T2 known times -> duration retained; T3 unknown
  // times -> no manufactured zero duration; T4 empty/error -> existing state.
  // T5 a linked JsonInspector peer installed by another workspace still uses
  // this renderer's React dispatcher; exercise its real state hook in T1.
  const render = (log: SessionEvent[], loadError: Error | null = null) => renderToStaticMarkup(
    createElement(MemoryRouter, null, createElement(TraceView, { log, loadError })),
  );
  it("keeps badges out of the marker rail and preserves duration and links", () => {
    const html = render([
      { id: "call", type: "agent.tool_use", name: "very_long_tool_name", input: {}, processed_at: "2026-09-06T00:00:00.100Z" },
      { id: "result", type: "agent.tool_result", tool_use_id: "call", is_error: true, content: [], processed_at: "2026-09-06T00:00:00.350Z" },
    ] as unknown as SessionEvent[]);
    expect(html.match(/class="ui-event-list__marker"><\/span>/g)).toHaveLength(2);
    expect(html).toMatch(/ui-event-list__metadata[^>]*>.*tool/);
    expect(html).toContain("very_long_tool_name");
    expect(html).toContain("250ms");
    expect(html).toContain("Link to event result");
  });
  it("does not fabricate timing for legacy events", () => {
    const html = render([{ id: "old", type: "session.status_idle", processed_at: null }] as unknown as SessionEvent[]);
    expect(html).not.toContain("0ms");
    expect(html).toContain("status");
    expect(render([])).toContain("No spans yet.");
    expect(render([], new Error("load failed"))).toContain("load failed");
  });
});
