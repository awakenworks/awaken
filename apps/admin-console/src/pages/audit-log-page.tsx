import { useState } from "react";
import { ConfigApiError, configApi } from "@/lib/config-api";
import {
  formatActor,
  summarizeChange,
  type AuditAction,
  type AuditEvent,
  type AuditPage,
} from "@/lib/audit-log";
import { useAuditFilterUrlState } from "@/lib/list-url-state";

const ACTION_OPTIONS: Array<{ value: AuditAction | ""; label: string }> = [
  { value: "", label: "All actions" },
  { value: "create", label: "Create" },
  { value: "update", label: "Update" },
  { value: "delete", label: "Delete" },
  { value: "restart", label: "Restart" },
  { value: "publish", label: "Publish" },
  { value: "restore", label: "Restore" },
];

const ACTION_BADGE: Record<AuditAction, string> = {
  create: "bg-emerald-100 text-emerald-800",
  update: "bg-blue-100 text-blue-800",
  delete: "bg-rose-100 text-rose-800",
  restart: "bg-amber-100 text-amber-800",
  publish: "bg-violet-100 text-violet-800",
  restore: "bg-purple-100 text-purple-800",
};

export function AuditLogPage() {
  const { apply, ...filter } = useAuditFilterUrlState();

  const [page, setPage] = useState<AuditPage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notConfigured, setNotConfigured] = useState(false);
  const [selectedEvent, setSelectedEvent] = useState<AuditEvent | null>(null);
  const [hasLoaded, setHasLoaded] = useState(false);

  async function load(cursor?: string) {
    setLoading(true);
    setError(null);
    setNotConfigured(false);
    try {
      const result = await configApi.auditLog({
        since: filter.since || undefined,
        until: filter.until || undefined,
        action: filter.action || undefined,
        resource: filter.resource || undefined,
        actor: filter.actor || undefined,
        cursor,
      });
      if (cursor) {
        setPage((prev) => ({
          items: [...(prev?.items ?? []), ...result.items],
          next_cursor: result.next_cursor,
        }));
      } else {
        setPage(result);
      }
      setHasLoaded(true);
    } catch (err) {
      if (err instanceof ConfigApiError && err.status === 503) {
        setNotConfigured(true);
      } else {
        setError(err instanceof Error ? err.message : String(err));
      }
    } finally {
      setLoading(false);
    }
  }

  const hasActiveFilters =
    filter.since || filter.until || filter.action || filter.resource || filter.actor;

  const emptyMessage =
    hasActiveFilters
      ? "No audit events match these filters."
      : "Audit log is empty.";

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <header className="mb-8">
        <p className="text-sm font-medium uppercase tracking-[0.2em] text-slate-500">
          Security & Compliance
        </p>
        <h2 className="mt-2 text-3xl font-semibold text-slate-950">Audit Log</h2>
        <p className="mt-2 max-w-2xl text-sm text-slate-600">
          Track create, update, delete, restart, and publish operations on all
          resources.
        </p>
      </header>

      {notConfigured && (
        <div className="mb-6 rounded-2xl border border-amber-200 bg-amber-50 p-4 text-sm text-amber-800 shadow-sm">
          Audit log is not enabled on this server.{" "}
          <a
            href="https://docs.awaken.dev/audit-log"
            target="_blank"
            rel="noopener noreferrer"
            className="font-medium underline hover:no-underline"
          >
            Learn how to enable it
          </a>
          .
        </div>
      )}

      {/* Filter bar */}
      <section className="mb-4 rounded-2xl border border-slate-200 bg-white p-4 shadow-sm">
        <div className="flex flex-wrap items-end gap-3">
          <label className="flex flex-col gap-1">
            <span className="text-xs text-slate-500">Since</span>
            <input
              type="datetime-local"
              value={filter.since}
              onChange={(e) => apply({ since: e.target.value })}
              className="rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
            />
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-slate-500">Until</span>
            <input
              type="datetime-local"
              value={filter.until}
              onChange={(e) => apply({ until: e.target.value })}
              className="rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
            />
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-slate-500">Action</span>
            <select
              value={filter.action}
              onChange={(e) => apply({ action: e.target.value as AuditAction | "" })}
              className="rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
            >
              {ACTION_OPTIONS.map((opt) => (
                <option key={opt.value} value={opt.value}>
                  {opt.label}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-slate-500">Resource</span>
            <input
              type="text"
              value={filter.resource}
              placeholder="e.g. agents/my-agent"
              onChange={(e) => apply({ resource: e.target.value })}
              className="w-48 rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
            />
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-slate-500">Actor</span>
            <input
              type="text"
              value={filter.actor}
              placeholder="hash prefix or label"
              onChange={(e) => apply({ actor: e.target.value })}
              className="w-44 rounded-xl border border-slate-300 px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
            />
          </label>

          <button
            type="button"
            onClick={() => void load()}
            disabled={loading}
            className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {loading ? "Loading…" : "Search"}
          </button>

          {hasActiveFilters && (
            <button
              type="button"
              onClick={() => {
                apply({ since: "", until: "", action: "", resource: "", actor: "" });
                setPage(null);
                setHasLoaded(false);
              }}
              className="rounded-xl border border-slate-300 px-4 py-2 text-sm font-medium text-slate-700 transition hover:bg-slate-50"
            >
              Clear
            </button>
          )}
        </div>
      </section>

      {error && (
        <div className="mb-4 rounded-2xl border border-rose-200 bg-rose-50 p-4 text-sm text-rose-700 shadow-sm">
          {error}
        </div>
      )}

      {hasLoaded && !notConfigured && (
        <section className="rounded-2xl border border-slate-200 bg-white shadow-sm">
          <table className="min-w-full text-sm">
            <thead className="bg-slate-50 text-left text-xs uppercase tracking-wide text-slate-500">
              <tr>
                <th className="px-4 py-3">Time</th>
                <th className="px-4 py-3">Actor</th>
                <th className="px-4 py-3">Action</th>
                <th className="px-4 py-3">Resource</th>
                <th className="px-4 py-3">Change</th>
                <th className="px-4 py-3"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-slate-200">
              {page?.items.length === 0 ? (
                <tr>
                  <td
                    colSpan={6}
                    className="px-4 py-8 text-center text-sm text-slate-500"
                  >
                    {emptyMessage}
                  </td>
                </tr>
              ) : (
                page?.items.map((event) => (
                  <AuditRow
                    key={event.id}
                    event={event}
                    onView={() => setSelectedEvent(event)}
                  />
                ))
              )}
            </tbody>
          </table>

          {page?.next_cursor && (
            <div className="border-t border-slate-200 px-4 py-3">
              <button
                type="button"
                onClick={() => void load(page.next_cursor)}
                disabled={loading}
                className="text-sm font-medium text-slate-700 transition hover:text-slate-950 disabled:opacity-60"
              >
                {loading ? "Loading…" : "Load more"}
              </button>
            </div>
          )}
        </section>
      )}

      {!hasLoaded && !loading && !notConfigured && (
        <div className="rounded-2xl border border-slate-200 bg-white p-8 text-center text-sm text-slate-500 shadow-sm">
          Set filters and click <strong>Search</strong> to load audit events.
        </div>
      )}

      {selectedEvent && (
        <EventPanel event={selectedEvent} onClose={() => setSelectedEvent(null)} />
      )}
    </div>
  );
}

function AuditRow({
  event,
  onView,
}: {
  event: AuditEvent;
  onView: () => void;
}) {
  const actor = formatActor(event.actor);
  const ts = new Date(event.ts);

  return (
    <tr className="hover:bg-slate-50">
      <td className="px-4 py-3 font-mono text-xs text-slate-700">
        {ts.toLocaleString()}
      </td>
      <td className="px-4 py-3 text-sm text-slate-700">
        <span className="font-mono text-xs">{actor.hash}</span>
        {actor.label && (
          <span className="ml-1 text-slate-500">/{actor.label}</span>
        )}
      </td>
      <td className="px-4 py-3">
        <span
          className={[
            "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium",
            ACTION_BADGE[event.action] ?? "bg-slate-100 text-slate-700",
          ].join(" ")}
        >
          {event.action}
        </span>
      </td>
      <td className="max-w-xs truncate px-4 py-3 font-mono text-xs text-slate-900">
        {event.resource}
      </td>
      <td className="max-w-xs truncate px-4 py-3 text-xs text-slate-600">
        {summarizeChange(event)}
      </td>
      <td className="px-4 py-3">
        <button
          type="button"
          onClick={onView}
          className="text-xs font-medium text-slate-500 transition hover:text-slate-900"
        >
          View
        </button>
      </td>
    </tr>
  );
}

function EventPanel({
  event,
  onClose,
}: {
  event: AuditEvent;
  onClose: () => void;
}) {
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
      <div className="flex h-full w-full max-w-2xl flex-col overflow-y-auto bg-white shadow-2xl md:max-w-xl">
        <div className="flex items-center justify-between border-b border-slate-200 px-6 py-4">
          <h3 className="text-base font-semibold text-slate-900">Audit event</h3>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md px-2 py-1 text-sm text-slate-500 hover:bg-slate-100"
          >
            Close
          </button>
        </div>

        <dl className="grid gap-3 px-6 py-4 text-sm">
          <Row label="ID">
            <span className="font-mono text-xs">{event.id}</span>
          </Row>
          <Row label="Time">
            <span className="font-mono text-xs">{event.ts}</span>
          </Row>
          <Row label="Actor">
            <span className="font-mono text-xs">{actor.hash}</span>
            {actor.label && <span className="ml-1 text-slate-500">/{actor.label}</span>}
          </Row>
          <Row label="Action">
            <span
              className={[
                "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium",
                ACTION_BADGE[event.action] ?? "bg-slate-100 text-slate-700",
              ].join(" ")}
            >
              {event.action}
            </span>
          </Row>
          <Row label="Resource">
            <span className="font-mono text-xs">{event.resource}</span>
          </Row>
          {event.ip && (
            <Row label="IP">
              <span className="font-mono text-xs">{event.ip}</span>
            </Row>
          )}
          {event.request_id && (
            <Row label="Request ID">
              <span className="font-mono text-xs">{event.request_id}</span>
            </Row>
          )}
        </dl>

        <div className="grid gap-4 px-6 pb-6 md:grid-cols-2">
          <div>
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-slate-500">
              Before
            </p>
            <pre className="overflow-auto rounded-xl border border-slate-200 bg-slate-50 p-3 text-xs leading-relaxed text-slate-800">
              {event.before != null
                ? JSON.stringify(event.before, null, 2)
                : <span className="text-slate-400">—</span>}
            </pre>
          </div>
          <div>
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-slate-500">
              After
            </p>
            <pre className="overflow-auto rounded-xl border border-slate-200 bg-slate-50 p-3 text-xs leading-relaxed text-slate-800">
              {event.after != null
                ? JSON.stringify(event.after, null, 2)
                : <span className="text-slate-400">—</span>}
            </pre>
          </div>
        </div>
      </div>
    </div>
  );
}

function Row({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex items-baseline gap-3">
      <dt className="w-24 shrink-0 text-xs font-medium text-slate-500">{label}</dt>
      <dd className="min-w-0 text-slate-900">{children}</dd>
    </div>
  );
}
