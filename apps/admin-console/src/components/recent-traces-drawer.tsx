import { useEffect, useState } from "react";
import { useInfiniteQuery, useQuery } from "@tanstack/react-query";
import { tracesApi, type TraceEvent, type TracePage, type TraceRunSummary } from "@/lib/config-api";
import { redactSecretsForDisplay } from "@/lib/agent-editor-helpers";
import { safeErrorMessage } from "@/lib/safe-error-message";

const TRACE_EVENT_PAGE_LIMIT = 1000;

/**
 * Side drawer that lists recent persisted runs for an agent and lets the
 * operator drill into one to see the full event stream.
 *
 * Backed by ADR-0030 endpoints:
 *   - `GET /v1/traces?agent_id=…` → list of `TraceRunSummary`
 *   - `GET /v1/traces/:run_id`    → NDJSON page of trace events
 *
 * The endpoints are feature-gated server-side (`expose_trace_routes`).
 * `tracesApi.listAgentTraces` returns `null` when the server isn't
 * configured for trace persistence; the drawer surfaces that as a
 * friendly "not configured" state rather than rendering an error.
 */
export function RecentTracesDrawer({
  agentId,
  open,
  onClose,
}: {
  agentId: string;
  open: boolean;
  onClose: () => void;
}) {
  const [selectedRunId, setSelectedRunId] = useState<string | null>(null);

  useEffect(() => {
    function onKey(event: KeyboardEvent) {
      if (event.key === "Escape") onClose();
    }
    if (open) {
      document.addEventListener("keydown", onKey);
      return () => document.removeEventListener("keydown", onKey);
    }
    return undefined;
  }, [open, onClose]);

  // Reset the selected run when the drawer is closed so reopening starts
  // from the list view.
  useEffect(() => {
    if (!open) setSelectedRunId(null);
  }, [open]);

  const listQuery = useQuery({
    queryKey: ["traces", "list", agentId],
    queryFn: () => tracesApi.listAgentTraces(agentId, { limit: 25 }),
    enabled: open && agentId.trim().length > 0,
    staleTime: 10_000,
  });

  if (!open) return null;

  return (
    <>
      <button
        type="button"
        aria-label="Close trace drawer"
        onClick={onClose}
        data-testid="recent-traces-drawer-scrim"
        className="fixed inset-0 z-40 bg-black/30"
      />
      <aside
        role="dialog"
        aria-label="Recent runs"
        data-testid="recent-traces-drawer"
        className="fixed right-0 top-0 z-50 flex h-full w-full max-w-xl flex-col border-l border-line bg-surface shadow-xl"
      >
        <header className="flex items-center justify-between border-b border-line px-4 py-3">
          <div>
            <h2 className="text-sm font-semibold text-fg-strong">Recent runs</h2>
            <p className="text-[11px] text-fg-soft">
              Agent <span className="font-mono">{agentId}</span> · latest 25
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md border border-line-strong bg-soft px-2 py-1 text-xs text-fg-soft transition hover:bg-muted"
          >
            Close
          </button>
        </header>

        <div className="min-h-0 flex-1 overflow-y-auto">
          {selectedRunId ? (
            <RunEventViewer runId={selectedRunId} onBack={() => setSelectedRunId(null)} />
          ) : (
            <RunList
              loading={listQuery.isPending && listQuery.fetchStatus === "fetching"}
              error={listQuery.error}
              data={listQuery.data}
              onSelect={(runId) => setSelectedRunId(runId)}
            />
          )}
        </div>
      </aside>
    </>
  );
}

function RunList({
  loading,
  error,
  data,
  onSelect,
}: {
  loading: boolean;
  error: unknown;
  data: { runs: TraceRunSummary[] } | null | undefined;
  onSelect: (runId: string) => void;
}) {
  if (loading) {
    return <div className="px-4 py-6 text-xs text-fg-soft">Loading recent runs…</div>;
  }
  if (error) {
    return (
      <div className="px-4 py-6 text-xs text-tone-error">
        Failed to load runs: {safeErrorMessage(error)}
      </div>
    );
  }
  if (data === null) {
    return (
      <div className="px-4 py-6 text-xs text-fg-soft" data-testid="recent-traces-not-configured">
        Trace persistence is not enabled on this server build (
        <span className="font-mono">expose_trace_routes=false</span> or no trace store configured).
        Ask the operator to enable trace persistence to populate this view.
      </div>
    );
  }
  if (!data || data.runs.length === 0) {
    return (
      <div className="px-4 py-6 text-xs text-fg-soft" data-testid="recent-traces-empty">
        No runs recorded for this agent yet. Run the sandbox or invoke the agent through the API and
        a summary will appear here.
      </div>
    );
  }
  return (
    <ul className="divide-y divide-line" data-testid="recent-traces-list">
      {data.runs.map((run) => (
        <li key={run.run_id}>
          <button
            type="button"
            onClick={() => onSelect(run.run_id)}
            className="flex w-full flex-col gap-1 px-4 py-3 text-left text-xs transition hover:bg-soft"
          >
            <div className="flex items-baseline justify-between gap-3">
              <span className="font-mono text-fg-strong">{run.run_id.slice(0, 16)}</span>
              <span className="text-fg-soft">{formatRelativeTime(run.started_at)}</span>
            </div>
            <div className="flex flex-wrap gap-2 text-[10px] text-fg-soft">
              {run.final_status ? (
                <span className="rounded-pill bg-muted px-2 py-0.5 font-mono">
                  {run.final_status}
                </span>
              ) : (
                <span className="rounded-pill bg-tone-warn/15 px-2 py-0.5 font-mono text-tone-warn">
                  in flight
                </span>
              )}
              {run.experiment_id ? (
                <span className="rounded-pill bg-muted px-2 py-0.5 font-mono">
                  exp: {run.experiment_id}
                </span>
              ) : null}
              {run.variant_name ? (
                <span className="rounded-pill bg-muted px-2 py-0.5 font-mono">
                  variant: {run.variant_name}
                </span>
              ) : null}
              {typeof run.judge_score === "number" ? (
                <span className="rounded-pill bg-muted px-2 py-0.5 font-mono">
                  judge: {run.judge_score.toFixed(2)}
                </span>
              ) : null}
            </div>
          </button>
        </li>
      ))}
    </ul>
  );
}

function RunEventViewer({ runId, onBack }: { runId: string; onBack: () => void }) {
  const eventsQuery = useInfiniteQuery({
    queryKey: ["traces", "events", runId],
    queryFn: ({ pageParam }) =>
      tracesApi.getTracePage(runId, {
        offset: pageParam,
        limit: TRACE_EVENT_PAGE_LIMIT,
      }),
    initialPageParam: 0,
    getNextPageParam: (lastPage) => lastPage?.next_offset ?? undefined,
    staleTime: 30_000,
  });
  const page = combineTracePages(eventsQuery.data?.pages);

  return (
    <div data-testid="recent-traces-events">
      <div className="flex items-center gap-2 border-b border-line px-4 py-2">
        <button
          type="button"
          onClick={onBack}
          className="rounded-md border border-line-strong bg-soft px-2 py-1 text-[11px] text-fg-soft transition hover:bg-muted"
        >
          ← Back to runs
        </button>
        <span className="font-mono text-[11px] text-fg-strong">{runId}</span>
      </div>
      {eventsQuery.isPending && eventsQuery.fetchStatus === "fetching" && page === undefined ? (
        <div className="px-4 py-6 text-xs text-fg-soft">Loading events…</div>
      ) : eventsQuery.error && page === undefined ? (
        <div className="px-4 py-6 text-xs text-tone-error">
          Failed to load events: {safeErrorMessage(eventsQuery.error)}
        </div>
      ) : page === null ? (
        // Server build lacks trace persistence (503). Distinct from
        // "0 events", which would be a successful empty page.
        <div
          className="px-4 py-6 text-xs text-fg-soft"
          data-testid="recent-traces-events-unavailable"
        >
          Trace persistence is not configured on this server build.
        </div>
      ) : (
        <EventList
          page={page ?? { events: [], total: 0, next_offset: null }}
          canLoadMore={eventsQuery.hasNextPage}
          loadingMore={eventsQuery.isFetchingNextPage}
          loadError={eventsQuery.error}
          onLoadMore={() => void eventsQuery.fetchNextPage()}
        />
      )}
    </div>
  );
}

function combineTracePages(
  pages: Array<TracePage | null> | undefined,
): TracePage | null | undefined {
  if (!pages) return undefined;
  if (pages.some((page) => page === null)) return null;
  const loadedPages = pages as TracePage[];
  const lastPage = loadedPages[loadedPages.length - 1];
  const events = loadedPages.flatMap((page) => page.events);
  return {
    events,
    total: lastPage?.total ?? events.length,
    next_offset: lastPage?.next_offset ?? null,
  };
}

function EventList({
  page,
  canLoadMore,
  loadingMore,
  loadError,
  onLoadMore,
}: {
  page: TracePage;
  canLoadMore: boolean;
  loadingMore: boolean;
  loadError: unknown;
  onLoadMore: () => void;
}) {
  return (
    <>
      <div className="px-4 py-2 text-[10px] uppercase tracking-eyebrow text-fg-soft">
        {page.events.length} of {page.total} events
        {canLoadMore ? " loaded" : ""}
      </div>
      <ul className="divide-y divide-line">
        {page.events.map((event, index) => (
          <li key={index} className="px-4 py-2 text-[11px]" data-testid="recent-traces-event-row">
            <EventRow event={event} />
          </li>
        ))}
      </ul>
      {loadError && page.events.length > 0 ? (
        <div className="px-4 py-2 text-[11px] text-tone-error">
          Failed to load more events: {safeErrorMessage(loadError)}
        </div>
      ) : null}
      {canLoadMore ? (
        <div className="border-t border-line px-4 py-3">
          <button
            type="button"
            onClick={onLoadMore}
            disabled={loadingMore}
            className="rounded-md border border-line-strong bg-surface px-3 py-1.5 text-xs font-medium text-fg-soft transition hover:bg-soft disabled:cursor-not-allowed disabled:opacity-60"
          >
            {loadingMore ? "Loading…" : "Load more"}
          </button>
        </div>
      ) : null}
    </>
  );
}

function EventRow({ event }: { event: TraceEvent }) {
  const kind = typeof event.kind === "string" ? event.kind : "unknown";
  // Trace event payloads can carry serialized agent specs in fields like
  // `agent_resolved.endpoint.auth`. Mask defensively before rendering;
  // the wire `event` is unchanged for any caller that re-reads it.
  const redacted = redactSecretsForDisplay(event);
  return (
    <details>
      <summary className="flex cursor-pointer items-center gap-2">
        <span className="rounded-pill bg-muted px-2 py-0.5 font-mono text-fg-soft">{kind}</span>
        {typeof event.ts === "number" ? (
          <span className="text-[10px] text-fg-faint">
            {new Date(event.ts * 1000).toISOString()}
          </span>
        ) : null}
      </summary>
      <pre className="mt-2 max-h-48 overflow-auto rounded-md bg-code-bg px-2 py-2 font-mono text-[10px] text-code-fg">
        {JSON.stringify(redacted, null, 2)}
      </pre>
    </details>
  );
}

function formatRelativeTime(seconds: number): string {
  const nowMs = Date.now();
  const thenMs = seconds * 1000;
  const deltaSec = Math.max(0, Math.round((nowMs - thenMs) / 1000));
  if (deltaSec < 60) return `${deltaSec}s ago`;
  if (deltaSec < 3600) return `${Math.round(deltaSec / 60)}m ago`;
  if (deltaSec < 86_400) return `${Math.round(deltaSec / 3600)}h ago`;
  return new Date(thenMs).toISOString().slice(0, 10);
}
