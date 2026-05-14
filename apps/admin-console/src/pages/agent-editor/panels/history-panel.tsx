import { useEffect, useState } from "react";
import { type AgentSpec, configApi } from "@/lib/config-api";
import { type AuditEvent, formatActor, summarizeChange } from "@/lib/audit-log";
import { useToast } from "@/components/toast-provider";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { useAuditLogInfiniteQuery } from "@/lib/query/hooks/audit";
import { safeErrorMessage } from "@/lib/safe-error-message";
import { hydrateAgentSpec } from "../spec-helpers";

const ACTION_BADGE: Record<string, string> = {
  create: "bg-tone-success/15 text-tone-success",
  update: "bg-blue-100 text-blue-800",
  delete: "bg-tone-error/15 text-tone-error",
  restart: "bg-tone-warn/15 text-tone-warn",
  publish: "bg-violet-100 text-violet-800",
  restore: "bg-purple-100 text-purple-800",
};

export function HistoryPanel({
  spec,
  isNew,
  refreshKey,
  onSpecRestored,
}: {
  spec: AgentSpec;
  isNew: boolean;
  refreshKey: number;
  onSpecRestored: (updated: AgentSpec) => void | Promise<void>;
}) {
  const toast = useToast();
  const confirm = useConfirmDialog();
  const [selectedEvent, setSelectedEvent] = useState<AuditEvent | null>(null);
  const [restoring, setRestoring] = useState<string | null>(null);
  const historyQuery = useAuditLogInfiniteQuery(
    { resource: `agents/${spec.id}`, limit: 50 },
    { enabled: !isNew && Boolean(spec.id) },
  );
  const page = historyQuery.data?.pages[0] ?? null;
  const loading = historyQuery.isFetching;
  const error = historyQuery.error ? safeErrorMessage(historyQuery.error) : null;
  const refetchHistory = historyQuery.refetch;

  useEffect(() => {
    if (refreshKey > 0) {
      void refetchHistory();
    }
  }, [refetchHistory, refreshKey]);

  async function handleRestore(event: AuditEvent) {
    const targetSpec = event.action === "delete" ? event.before : event.after;
    const confirmed = await confirm({
      title: "Restore agent to this version?",
      description: (
        <div className="space-y-3">
          <p className="text-xs text-fg-soft">
            Restoring will overwrite the current agent configuration with the version from this
            event.
          </p>
          <div className="grid grid-cols-2 gap-3">
            <div>
              <p className="mb-1 text-xs font-medium uppercase tracking-wide text-fg-soft">
                Current
              </p>
              <pre className="max-h-48 overflow-auto rounded-xl border border-line bg-soft p-2 text-xs text-fg">
                {JSON.stringify(spec, null, 2)}
              </pre>
            </div>
            <div>
              <p className="mb-1 text-xs font-medium uppercase tracking-wide text-fg-soft">
                This version
              </p>
              <pre className="max-h-48 overflow-auto rounded-xl border border-line bg-soft p-2 text-xs text-fg">
                {targetSpec != null ? JSON.stringify(targetSpec, null, 2) : "—"}
              </pre>
            </div>
          </div>
        </div>
      ),
      confirmLabel: "Restore",
      tone: "destructive",
    });

    if (!confirmed) return;

    setRestoring(event.id);
    try {
      await configApi.restoreConfig("agents", spec.id, event.id);
      const shortId = event.id.slice(0, 8);
      toast.success(`Agent restored to version ${shortId}`);
      const refreshed = await configApi.get<AgentSpec>("agents", spec.id);
      const hydrated = hydrateAgentSpec(refreshed);
      await onSpecRestored(hydrated);
      void refetchHistory();
    } catch (err) {
      toast.error(safeErrorMessage(err));
    } finally {
      setRestoring(null);
    }
  }

  if (isNew || !spec.id) {
    return (
      <section className="rounded-md border border-dashed border-line bg-surface p-6 text-center text-sm text-fg-soft shadow-sm">
        Save the agent first to see its history.
      </section>
    );
  }

  return (
    <section className="rounded-md border border-line bg-surface shadow-sm">
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h3 className="text-lg font-semibold text-fg-strong">History</h3>
        <button
          type="button"
          onClick={() => void refetchHistory()}
          disabled={loading}
          className="text-xs font-medium text-fg-soft transition hover:text-fg-strong disabled:opacity-60"
        >
          {loading ? "Loading…" : "Refresh"}
        </button>
      </div>

      {error && <div className="px-5 py-3 text-sm text-tone-error">{error}</div>}

      {!error && page && (
        <table className="min-w-full text-sm">
          <thead className="bg-soft text-left text-xs uppercase tracking-wide text-fg-soft">
            <tr>
              <th className="px-4 py-3">Time</th>
              <th className="px-4 py-3">Actor</th>
              <th className="px-4 py-3">Action</th>
              <th className="px-4 py-3">Change</th>
              <th className="px-4 py-3"></th>
            </tr>
          </thead>
          <tbody className="divide-y divide-line">
            {page.items.length === 0 ? (
              <tr>
                <td colSpan={5} className="px-4 py-8 text-center text-sm text-fg-soft">
                  No history yet.
                </td>
              </tr>
            ) : (
              page.items.map((event) => {
                const actor = formatActor(event.actor);
                const ts = new Date(event.ts);
                return (
                  <tr key={event.id} className="hover:bg-soft">
                    <td className="px-4 py-3 font-mono text-xs text-fg">{ts.toLocaleString()}</td>
                    <td className="px-4 py-3 text-sm text-fg">
                      <span className="font-mono text-xs">{actor.hash}</span>
                      {actor.label && <span className="ml-1 text-fg-soft">/{actor.label}</span>}
                    </td>
                    <td className="px-4 py-3">
                      <span
                        className={[
                          "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium",
                          ACTION_BADGE[event.action] ?? "bg-muted text-fg",
                        ].join(" ")}
                      >
                        {event.action}
                      </span>
                    </td>
                    <td className="max-w-xs truncate px-4 py-3 text-xs text-fg-soft">
                      {summarizeChange(event)}
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex items-center gap-3">
                        <button
                          type="button"
                          onClick={() => setSelectedEvent(event)}
                          className="text-xs font-medium text-fg-soft transition hover:text-fg-strong"
                        >
                          View
                        </button>
                        {event.action !== "restart" && (
                          <button
                            type="button"
                            onClick={() => void handleRestore(event)}
                            disabled={restoring === event.id}
                            className="text-xs font-medium text-tone-error transition hover:text-tone-error disabled:opacity-60"
                          >
                            {restoring === event.id ? "Restoring…" : "Restore"}
                          </button>
                        )}
                      </div>
                    </td>
                  </tr>
                );
              })
            )}
          </tbody>
        </table>
      )}

      {selectedEvent && (
        <HistoryEventPanel event={selectedEvent} onClose={() => setSelectedEvent(null)} />
      )}
    </section>
  );
}

function HistoryEventPanel({ event, onClose }: { event: AuditEvent; onClose: () => void }) {
  const actor = formatActor(event.actor);
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Audit event details"
      className="fixed inset-0 z-50 flex items-start justify-end bg-black/30"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="flex h-full w-full max-w-2xl flex-col overflow-y-auto bg-surface shadow-2xl md:max-w-xl">
        <div className="flex items-center justify-between border-b border-line px-6 py-4">
          <h3 className="text-base font-semibold text-fg-strong">Audit event</h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-2 py-1 text-sm text-fg-soft hover:bg-muted"
          >
            Close
          </button>
        </div>

        <dl className="grid gap-3 px-6 py-4 text-sm">
          <div className="flex items-baseline gap-3">
            <dt className="w-24 shrink-0 text-xs font-medium text-fg-soft">ID</dt>
            <dd className="min-w-0 font-mono text-xs text-fg-strong">{event.id}</dd>
          </div>
          <div className="flex items-baseline gap-3">
            <dt className="w-24 shrink-0 text-xs font-medium text-fg-soft">Time</dt>
            <dd className="min-w-0 font-mono text-xs text-fg-strong">{event.ts}</dd>
          </div>
          <div className="flex items-baseline gap-3">
            <dt className="w-24 shrink-0 text-xs font-medium text-fg-soft">Actor</dt>
            <dd className="min-w-0 text-fg-strong">
              <span className="font-mono text-xs">{actor.hash}</span>
              {actor.label && <span className="ml-1 text-fg-soft">/{actor.label}</span>}
            </dd>
          </div>
          <div className="flex items-baseline gap-3">
            <dt className="w-24 shrink-0 text-xs font-medium text-fg-soft">Action</dt>
            <dd className="min-w-0">
              <span
                className={[
                  "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium",
                  ACTION_BADGE[event.action] ?? "bg-muted text-fg",
                ].join(" ")}
              >
                {event.action}
              </span>
            </dd>
          </div>
        </dl>

        <div className="grid gap-4 px-6 pb-6 md:grid-cols-2">
          <div>
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-fg-soft">Before</p>
            <pre className="overflow-auto rounded-xl border border-line bg-soft p-3 text-xs leading-relaxed text-fg">
              {event.before != null ? JSON.stringify(event.before, null, 2) : "—"}
            </pre>
          </div>
          <div>
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-fg-soft">After</p>
            <pre className="overflow-auto rounded-xl border border-line bg-soft p-3 text-xs leading-relaxed text-fg">
              {event.after != null ? JSON.stringify(event.after, null, 2) : "—"}
            </pre>
          </div>
        </div>
      </div>
    </div>
  );
}
