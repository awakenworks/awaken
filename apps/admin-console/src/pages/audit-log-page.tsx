import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { ConfigApiError } from "@/lib/api";

/** Quote a CSV cell per RFC 4180 (double-quote any cell containing , " or \n). */
function csvCell(v: string): string {
  if (/[",\n]/.test(v)) return `"${v.replace(/"/g, '""')}"`;
  return v;
}
import {
  formatActor,
  isAgentActor,
  summarizeChange,
  type AuditAction,
  type AuditEvent,
  type AuditPage,
  type AuditQuery,
} from "@/lib/audit-log";
import { useAuditFilterUrlState } from "@/lib/list-url-state";
import { useAuditLogInfiniteQuery } from "@/lib/query/hooks/audit";

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
  create: "bg-tone-success/15 text-tone-success",
  update: "bg-blue-100 text-blue-800",
  delete: "bg-tone-error/15 text-tone-error",
  restart: "bg-tone-warn/15 text-tone-warn",
  publish: "bg-violet-100 text-violet-800",
  restore: "bg-purple-100 text-purple-800",
};

type AuditFilterState = Omit<ReturnType<typeof useAuditFilterUrlState>, "apply">;

function toAuditQuery(filter: AuditFilterState): AuditQuery {
  return {
    since: filter.since || undefined,
    until: filter.until || undefined,
    action: filter.action || undefined,
    resource: filter.resource || undefined,
    actor: filter.actor || undefined,
  };
}

export function AuditLogPage() {
  const { t } = useTranslation();
  const { apply, ...filter } = useAuditFilterUrlState();

  const [submittedFilter, setSubmittedFilter] = useState(filter);
  const [selectedEvent, setSelectedEvent] = useState<AuditEvent | null>(null);
  const auditQuery = useAuditLogInfiniteQuery(toAuditQuery(submittedFilter));
  const page = useMemo<AuditPage | null>(() => {
    if (!auditQuery.data) return null;
    const items = auditQuery.data.pages.flatMap((p) => p.items);
    const lastPage = auditQuery.data.pages[auditQuery.data.pages.length - 1];
    return { items, next_cursor: lastPage?.next_cursor };
  }, [auditQuery.data]);
  const loading = auditQuery.isFetching;
  const hasLoaded = auditQuery.data !== undefined;
  const notConfigured =
    auditQuery.error instanceof ConfigApiError && auditQuery.error.status === 503;
  const error =
    auditQuery.error && !notConfigured
      ? auditQuery.error instanceof Error
        ? auditQuery.error.message
        : String(auditQuery.error)
      : null;

  function load(override?: Partial<typeof filter>) {
    setSubmittedFilter({ ...filter, ...(override ?? {}) });
  }

  const hasActiveFilters =
    filter.since || filter.until || filter.action || filter.resource || filter.actor;

  const emptyMessage = hasActiveFilters ? t("audit.noMatches") : t("audit.empty");

  function handleExportCsv() {
    if (!page || page.items.length === 0) return;
    const rows: string[][] = [
      ["time", "actor", "action", "resource", "summary"],
      ...page.items.map((e) => [
        e.ts,
        e.actor ?? "system",
        e.action,
        e.resource,
        summarizeChange(e),
      ]),
    ];
    const csv = rows.map((r) => r.map(csvCell).join(",")).join("\n") + "\n";
    const blob = new Blob([csv], { type: "text/csv;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `audit-log-${new Date().toISOString().slice(0, 10)}.csv`;
    document.body.appendChild(a);
    a.click();
    document.body.removeChild(a);
    URL.revokeObjectURL(url);
  }

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <header className="mb-4 flex items-baseline justify-between gap-3">
        <div className="flex items-baseline gap-3">
          <h2 className="text-2xl font-semibold tracking-title-em text-fg-strong">
            {t("audit.title")}
          </h2>
          {page && (
            <span aria-hidden className="font-mono text-sm text-fg-faint">
              {page.items.length}
              {page.next_cursor ? "+" : ""}
            </span>
          )}
        </div>
        {page && page.items.length > 0 && (
          <button
            type="button"
            onClick={handleExportCsv}
            className="inline-flex h-8 items-center gap-1.5 rounded-md border border-line-strong bg-surface px-2.5 text-xs font-medium text-fg-soft transition hover:bg-soft hover:text-fg"
          >
            ⤓ {t("audit.exportCsv")}
          </button>
        )}
      </header>

      {notConfigured && (
        <div className="mb-6 rounded-md border border-tone-warn/35 bg-tone-warn/10 p-4 text-sm text-tone-warn shadow-sm">
          {t("audit.notConfigured")}
        </div>
      )}

      {/* Filter bar */}
      <section className="mb-4 rounded-md border border-line bg-surface p-4 shadow-sm">
        <div className="flex flex-wrap items-end gap-3">
          <label className="flex flex-col gap-1">
            <span className="text-xs text-fg-soft">{t("audit.filters.since")}</span>
            <input
              type="datetime-local"
              value={filter.since}
              onChange={(e) => apply({ since: e.target.value })}
              className="rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
            />
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-fg-soft">{t("audit.filters.until")}</span>
            <input
              type="datetime-local"
              value={filter.until}
              onChange={(e) => apply({ until: e.target.value })}
              className="rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
            />
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-fg-soft">{t("audit.filters.action")}</span>
            <select
              value={filter.action}
              onChange={(e) => apply({ action: e.target.value as AuditAction | "" })}
              className="rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
            >
              {ACTION_OPTIONS.map((opt) => (
                <option key={opt.value} value={opt.value}>
                  {opt.label}
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-fg-soft">{t("audit.filters.resource")}</span>
            <input
              type="text"
              value={filter.resource}
              placeholder="e.g. agents/my-agent"
              onChange={(e) => apply({ resource: e.target.value })}
              className="w-48 rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
            />
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-xs text-fg-soft">{t("audit.filters.actor")}</span>
            <input
              type="text"
              value={filter.actor}
              placeholder="hash prefix or label"
              onChange={(e) => apply({ actor: e.target.value })}
              className="w-44 rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
            />
          </label>

          <button
            type="button"
            onClick={() => load()}
            disabled={loading}
            className="rounded-xl bg-accent px-4 py-2 text-sm font-medium text-accent-text transition hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-60"
          >
            {loading ? t("audit.filters.loading") : t("audit.filters.search")}
          </button>

          {hasActiveFilters && (
            <button
              type="button"
              onClick={() => {
                const empty = {
                  since: "",
                  until: "",
                  action: "" as const,
                  resource: "",
                  actor: "",
                };
                apply(empty);
                load(empty);
              }}
              className="rounded-xl border border-line-strong px-4 py-2 text-sm font-medium text-fg transition hover:bg-soft"
            >
              {t("audit.filters.clear")}
            </button>
          )}
        </div>
      </section>

      {error && (
        <div className="mb-4 rounded-md border border-tone-error/30 bg-tone-error/10 p-4 text-sm text-tone-error shadow-sm">
          {error}
        </div>
      )}

      {hasLoaded && !notConfigured && (
        <section className="rounded-md border border-line bg-surface shadow-sm">
          <table className="min-w-full text-sm">
            <thead className="bg-soft text-left text-xs uppercase tracking-wide text-fg-soft">
              <tr>
                <th className="px-4 py-3">{t("audit.columns.time")}</th>
                <th className="px-4 py-3">{t("audit.columns.actor")}</th>
                <th className="px-4 py-3">{t("audit.columns.action")}</th>
                <th className="px-4 py-3">{t("audit.columns.resource")}</th>
                <th className="px-4 py-3">{t("audit.columns.change")}</th>
                <th className="px-4 py-3"></th>
              </tr>
            </thead>
            <tbody className="divide-y divide-line">
              {page?.items.length === 0 ? (
                <tr>
                  <td colSpan={6} className="px-4 py-8 text-center text-sm text-fg-soft">
                    {emptyMessage}
                  </td>
                </tr>
              ) : (
                page?.items.map((event) => (
                  <AuditRow key={event.id} event={event} onView={() => setSelectedEvent(event)} />
                ))
              )}
            </tbody>
          </table>

          {page?.next_cursor && (
            <div className="border-t border-line px-4 py-3">
              <button
                type="button"
                onClick={() => void auditQuery.fetchNextPage()}
                disabled={loading || !auditQuery.hasNextPage}
                className="text-sm font-medium text-fg transition hover:text-fg-strong disabled:opacity-60"
              >
                {loading ? "Loading…" : "Load more"}
              </button>
            </div>
          )}
        </section>
      )}

      {!hasLoaded && loading && !notConfigured && (
        <div className="rounded-md border border-line bg-surface p-8 text-center text-sm text-fg-soft shadow-sm">
          Loading audit events…
        </div>
      )}

      {selectedEvent && <EventPanel event={selectedEvent} onClose={() => setSelectedEvent(null)} />}
    </div>
  );
}

function AuditRow({ event, onView }: { event: AuditEvent; onView: () => void }) {
  const actor = formatActor(event.actor);
  const ts = new Date(event.ts);
  const fromAgent = isAgentActor(event.actor);

  return (
    <tr
      className={
        fromAgent
          ? "border-l-2 border-agent-stripe bg-agent-tint hover:bg-agent-tint/80"
          : "hover:bg-soft"
      }
    >
      <td className="px-4 py-3 font-mono text-xs text-fg">{ts.toLocaleString()}</td>
      <td className="px-4 py-3 text-sm text-fg" title={event.actor}>
        <span className="font-mono text-xs">{actor.hash.slice(0, 8)}</span>
        {actor.label && (
          <span
            className={["ml-1", fromAgent ? "font-medium text-agent-fg" : "text-fg-soft"].join(" ")}
          >
            /{actor.label}
          </span>
        )}
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
      <td className="max-w-xs truncate px-4 py-3 font-mono text-xs text-fg-strong">
        {event.resource}
      </td>
      <td className="max-w-xs truncate px-4 py-3 text-xs text-fg-soft">{summarizeChange(event)}</td>
      <td className="px-4 py-3">
        <button
          type="button"
          onClick={onView}
          className="text-xs font-medium text-fg-soft transition hover:text-fg-strong"
        >
          View
        </button>
      </td>
    </tr>
  );
}

function EventPanel({ event, onClose }: { event: AuditEvent; onClose: () => void }) {
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
          <Row label="ID">
            <span className="font-mono text-xs">{event.id}</span>
          </Row>
          <Row label="Time">
            <span className="font-mono text-xs">{event.ts}</span>
          </Row>
          <Row label="Actor">
            <span className="font-mono text-xs">{actor.hash}</span>
            {actor.label && <span className="ml-1 text-fg-soft">/{actor.label}</span>}
          </Row>
          <Row label="Action">
            <span
              className={[
                "inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium",
                ACTION_BADGE[event.action] ?? "bg-muted text-fg",
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
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-fg-soft">Before</p>
            <pre className="overflow-auto rounded-xl border border-line bg-soft p-3 text-xs leading-relaxed text-fg">
              {event.before != null ? (
                JSON.stringify(event.before, null, 2)
              ) : (
                <span className="text-fg-faint">—</span>
              )}
            </pre>
          </div>
          <div>
            <p className="mb-2 text-xs font-medium uppercase tracking-wide text-fg-soft">After</p>
            <pre className="overflow-auto rounded-xl border border-line bg-soft p-3 text-xs leading-relaxed text-fg">
              {event.after != null ? (
                JSON.stringify(event.after, null, 2)
              ) : (
                <span className="text-fg-faint">—</span>
              )}
            </pre>
          </div>
        </div>
      </div>
    </div>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-baseline gap-3">
      <dt className="w-24 shrink-0 text-xs font-medium text-fg-soft">{label}</dt>
      <dd className="min-w-0 text-fg-strong">{children}</dd>
    </div>
  );
}
