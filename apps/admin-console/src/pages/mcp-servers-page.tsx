import { useCallback, useEffect, useMemo, useState } from "react";
import { Link } from "react-router";
import {
  type McpRestartPolicy,
  type McpServerRecord,
  type McpServerSpec,
  configApi,
} from "@/lib/config-api";
import { useCrudPage } from "@/lib/use-crud-page";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { useToast } from "@/components/toast-provider";
import { Field } from "@/components/form-components";
import { EmptyState } from "@/components/ui/empty-state";
import { SecretField, SecretStatusPill } from "@/components/ui/secret-field";
import { SkeletonRows } from "@/components/ui/skeleton";
import {
  ListSearchBar,
  PageSizeSelect,
  Pagination,
  SortableHeader,
  type SortableColumn,
} from "@/components/list-controls";
import {
  compareNumber,
  compareString,
  filterBySearch,
  paginate,
  sortItems,
  toggleSort,
  type SortConfig,
} from "@/lib/list-view";
import { useListUrlState } from "@/lib/list-url-state";
import { formatRelativeTime } from "@/lib/format-time";
import { adminRoutes } from "@/lib/routes";
import {
  parseJsonObject,
  parseLineList,
  parseStringRecord,
  stringifyJsonObject,
  stringifyLineList,
} from "@/lib/config-form-helpers";

type McpSortKey = "id" | "transport" | "endpoint" | "updated_at";

function endpointFor(server: McpServerRecord): string {
  if (server.transport === "stdio") {
    return [server.command, ...(server.args ?? [])].filter(Boolean).join(" ");
  }
  return server.url ?? "";
}

const SORT_CONFIG: SortConfig<McpServerRecord, McpSortKey> = {
  id: (a, b) => compareString(a.id, b.id),
  transport: (a, b) => compareString(a.transport, b.transport),
  endpoint: (a, b) => compareString(endpointFor(a), endpointFor(b)),
  updated_at: (a, b) => compareNumber(a.updated_at ?? 0, b.updated_at ?? 0),
};

const COLUMNS: SortableColumn<McpSortKey>[] = [
  { key: "id", label: "ID" },
  { key: "transport", label: "Transport" },
  { key: "endpoint", label: "Endpoint" },
  { key: null, label: "Environment" },
  { key: "updated_at", label: "Last modified" },
  { key: null, label: "Status" },
  { key: null, label: "Actions" },
];

const LIST_OPTIONS = {
  validSortKeys: ["id", "transport", "endpoint", "updated_at"] as const,
  defaultSort: { key: "id" as McpSortKey, direction: "asc" as const },
} as const;

type EnvMode = "preserve" | "replace" | "clear";

const DEFAULT_RESTART_POLICY: McpRestartPolicy = {
  enabled: false,
  delay_ms: 1000,
  backoff_multiplier: 2,
  max_delay_ms: 30000,
};

const EMPTY_SERVER: McpServerRecord = {
  id: "",
  transport: "stdio",
  command: "",
  args: [],
  timeout_secs: 30,
  config: {},
  restart_policy: { ...DEFAULT_RESTART_POLICY },
};

import type { McpServerStatusResponse } from "@/lib/config-api";
type McpServerStatus = McpServerStatusResponse;

function StatusBadge({ status }: { status: McpServerStatus | null | undefined }) {
  if (status === undefined) {
    return (
      <span className="inline-block h-2 w-2 rounded-pill bg-fg-faint" title="Loading status..." />
    );
  }
  if (status === null) {
    return (
      <span className="inline-block h-2 w-2 rounded-pill bg-fg-faint" title="Status unavailable" />
    );
  }
  if (status.connected) {
    return (
      <span className="inline-block h-2 w-2 rounded-pill bg-state-done" title="Connected" />
    );
  }
  return (
    <span
      className="inline-block h-2 w-2 rounded-pill bg-state-blocked"
      title={status.last_error ? `Error: ${status.last_error}` : "Disconnected"}
    />
  );
}

export function McpServersPage() {
  const [argsDraft, setArgsDraft] = useState("");
  const [configDraft, setConfigDraft] = useState("{}");
  const [envDraft, setEnvDraft] = useState("{}");
  const [envMode, setEnvMode] = useState<EnvMode>("replace");
  const [statuses, setStatuses] = useState<Record<string, McpServerStatus | null>>({});
  const [restarting, setRestarting] = useState(false);
  const confirm = useConfirmDialog();
  const toast = useToast();

  const prepareSave = useCallback(
    (draft: McpServerRecord): McpServerSpec => {
      const payload: McpServerSpec = {
        ...draft,
        command: draft.transport === "stdio" ? String(draft.command ?? "") : undefined,
        url: draft.transport === "http" ? String(draft.url ?? "") : undefined,
        args: draft.transport === "stdio" ? parseLineList(argsDraft) : [],
        config: parseJsonObject<Record<string, unknown>>(configDraft, "Config JSON"),
        timeout_secs: Number(draft.timeout_secs) || 30,
        restart_policy: {
          ...DEFAULT_RESTART_POLICY,
          ...(draft.restart_policy ?? {}),
        },
      };

      if (envMode === "replace") {
        payload.env = parseStringRecord(envDraft, "Environment JSON");
      } else if (envMode === "clear") {
        payload.env = {};
      }

      return payload;
    },
    [argsDraft, configDraft, envMode, envDraft],
  );

  const crud = useCrudPage<McpServerRecord, McpServerSpec>({
    namespace: "mcp-servers",
    entityLabel: "MCP server",
    prepareSave,
  });

  const { search, sort, pageSize, page, apply: applyListState } = useListUrlState<McpSortKey>(LIST_OPTIONS);

  // Fetch live status for all loaded servers in parallel.
  useEffect(() => {
    if (crud.items.length === 0) return;
    const ids = crud.items.map((s) => s.id);
    // Initialise to undefined (loading) for unknown ids.
    setStatuses((prev) => {
      const next = { ...prev };
      for (const id of ids) {
        if (!(id in next)) next[id] = undefined as unknown as null;
      }
      return next;
    });
    void Promise.allSettled(
      ids.map((id) =>
        configApi.mcpStatus(id).then(
          (s) => setStatuses((prev) => ({ ...prev, [id]: s })),
          () => setStatuses((prev) => ({ ...prev, [id]: null })),
        ),
      ),
    );
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [crud.items.length]);

  async function handleRestart(id: string) {
    const ok = await confirm({
      title: "Restart MCP server?",
      description: `This will immediately reconnect "${id}". In-flight tool calls may be interrupted.`,
      confirmLabel: "Restart",
    });
    if (!ok) return;
    setRestarting(true);
    try {
      await configApi.mcpRestart(id);
      toast.push({ message: `MCP server "${id}" restart triggered.`, tone: "success" });
      // Re-fetch status after restart.
      try {
        const s = await configApi.mcpStatus(id);
        setStatuses((prev) => ({ ...prev, [id]: s }));
      } catch {
        setStatuses((prev) => ({ ...prev, [id]: null }));
      }
    } catch (err) {
      toast.push({ message: `Restart failed: ${err instanceof Error ? err.message : String(err)}`, tone: "error" });
    } finally {
      setRestarting(false);
    }
  }

  const filtered = useMemo(
    () =>
      filterBySearch(crud.items, search, (server) => [
        server.id,
        server.transport,
        endpointFor(server),
        ...(server.env_keys ?? []),
      ]),
    [crud.items, search],
  );
  const sorted = useMemo(
    () => sortItems(filtered, sort, SORT_CONFIG),
    [filtered, sort],
  );
  const view = useMemo(
    () => paginate(sorted, { page, pageSize, totalItems: sorted.length }),
    [sorted, page, pageSize],
  );

  useEffect(() => {
    if (view.page !== page) applyListState({ page: view.page });
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [view.page, page]);

  function startCreate() {
    crud.startNew({
      ...EMPTY_SERVER,
      restart_policy: { ...DEFAULT_RESTART_POLICY },
    });
    setArgsDraft("");
    setConfigDraft("{}");
    setEnvDraft("{}");
    setEnvMode("replace");
  }

  function startEdit(server: McpServerRecord) {
    crud.startEdit({
      ...server,
      command: String(server.command ?? ""),
      url: String(server.url ?? ""),
      args: [...(server.args ?? [])],
      config: { ...(server.config ?? {}) },
      restart_policy: {
        ...DEFAULT_RESTART_POLICY,
        ...(server.restart_policy ?? {}),
      },
    });
    setArgsDraft(stringifyLineList(server.args));
    setConfigDraft(stringifyJsonObject(server.config));
    setEnvDraft("{}");
    setEnvMode("preserve");
  }

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="mb-6 flex items-center justify-between gap-4">
        <div>
          <p className="text-sm font-medium uppercase tracking-[0.2em] text-fg-soft">
            Runtime Catalog
          </p>
          <h2 className="mt-2 text-3xl font-semibold text-fg-strong">
            MCP Servers
          </h2>
        </div>
        <button
          type="button"
          onClick={startCreate}
          className="rounded-xl bg-fg-strong px-4 py-2 text-sm font-medium text-bg transition hover:bg-fg"
        >
          New MCP Server
        </button>
      </div>

      {crud.draft ? (
        <section className="mb-6 rounded-2xl border border-line bg-surface p-5 shadow-sm">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-fg-strong">
              {crud.isEditingExisting ? "Edit MCP server" : "Create MCP server"}
            </h3>
            {crud.isEditingExisting && crud.draft.id && (
              <Link
                to={adminRoutes.auditLogForResource(`mcp-servers/${crud.draft.id}`)}
                className="text-sm font-medium text-fg-soft transition hover:text-fg"
              >
                History
              </Link>
            )}
          </div>

          <div className="mt-4 grid gap-4 md:grid-cols-2">
            <Field label="Server ID">
              <input
                value={crud.draft.id}
                disabled={crud.isEditingExisting}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current ? { ...current, id: event.target.value } : current,
                  )
                }
                className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong disabled:bg-muted disabled:text-fg-soft"
              />
            </Field>

            <Field label="Transport">
              <select
                value={crud.draft.transport}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current
                      ? {
                          ...current,
                          transport: event.target.value as "stdio" | "http",
                        }
                      : current,
                  )
                }
                className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
              >
                <option value="stdio">stdio</option>
                <option value="http">http</option>
              </select>
            </Field>

            {crud.draft.transport === "stdio" ? (
              <>
                <Field label="Command">
                  <input
                    value={String(crud.draft.command ?? "")}
                    onChange={(event) =>
                      crud.setDraft((current) =>
                        current
                          ? { ...current, command: event.target.value }
                          : current,
                      )
                    }
                    className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
                  />
                </Field>
                <Field label="Arguments (one per line)">
                  <textarea
                    value={argsDraft}
                    onChange={(event) => setArgsDraft(event.target.value)}
                    rows={5}
                    className="w-full rounded-xl border border-line-strong px-3 py-2 font-mono text-sm text-fg-strong outline-none transition focus:border-line-strong"
                  />
                </Field>
              </>
            ) : (
              <Field label="URL">
                <input
                  value={String(crud.draft.url ?? "")}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current ? { ...current, url: event.target.value } : current,
                    )
                  }
                  className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
                />
              </Field>
            )}

            <Field label="Timeout (seconds)">
              <input
                type="number"
                min={1}
                value={Number(crud.draft.timeout_secs ?? 30)}
                onChange={(event) =>
                  crud.setDraft((current) =>
                    current
                      ? {
                          ...current,
                          timeout_secs: Number(event.target.value) || 30,
                        }
                      : current,
                  )
                }
                className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
              />
            </Field>
          </div>

          <div className="mt-5 grid gap-4 lg:grid-cols-2">
            <Field label="Config JSON">
              <textarea
                value={configDraft}
                onChange={(event) => setConfigDraft(event.target.value)}
                rows={8}
                className="w-full rounded-xl border border-line-strong px-3 py-2 font-mono text-sm text-fg-strong outline-none transition focus:border-line-strong"
              />
            </Field>

            <SecretField
              mode={envMode === "preserve" ? "keep" : envMode}
              onModeChange={(next) =>
                setEnvMode(next === "keep" ? "preserve" : next)
              }
              currentlyHasValue={Boolean(crud.isEditingExisting && crud.draft.has_env)}
              statusPill={
                crud.draft.has_env ? (
                  <SecretStatusPill
                    state={envMode === "clear" ? "will-clear" : "stored"}
                  />
                ) : (
                  <SecretStatusPill state="no-value" />
                )
              }
              labels={{
                title: `MCP env${crud.draft.id ? ` — ${crud.draft.id}` : ""}`,
                description:
                  crud.isEditingExisting && crud.draft.has_env
                    ? `Stored keys: ${(crud.draft.env_keys ?? []).join(", ") || "(opaque)"}. Three-mode JSON editor mirrors the API-key pattern but accepts a flat object literal.`
                    : "Provide a flat JSON object of environment variables.",
                replaceLabel: "Replace JSON",
                clearLabel: "Clear env",
                keepBody: (
                  <>
                    <strong>Existing env is preserved.</strong>{" "}
                    <span>Save will not touch the stored variables.</span>
                  </>
                ),
                clearBody: (
                  <>
                    <strong>All env variables will be removed on save.</strong>{" "}
                    <span>The MCP process will start with no inherited env.</span>
                  </>
                ),
              }}
              hint={
                <>
                  Must parse as a flat <code className="font-mono">{`{[k]: string}`}</code> object. Validation runs on save; invalid JSON surfaces a 400 error from the server.
                </>
              }
            >
              <textarea
                value={envDraft}
                onChange={(event) => setEnvDraft(event.target.value)}
                rows={8}
                className="w-full rounded-md border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg-strong outline-none transition-colors focus:border-link"
              />
            </SecretField>
          </div>

          <section className="mt-5 rounded-xl border border-line bg-soft p-4">
            <div className="flex items-center justify-between gap-4">
              <div>
                <h4 className="text-sm font-semibold text-fg-strong">Restart Policy</h4>
                <p className="mt-1 text-sm text-fg-soft">
                  Controls automatic reconnection when the server becomes unavailable.
                </p>
              </div>
              <label className="flex items-center gap-2 text-sm font-medium text-fg">
                <input
                  type="checkbox"
                  checked={Boolean(crud.draft.restart_policy?.enabled)}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              enabled: event.target.checked,
                            },
                          }
                        : current,
                    )
                  }
                />
                Enabled
              </label>
            </div>

            <div
              className={[
                "mt-4 grid gap-4 md:grid-cols-2 xl:grid-cols-4 transition-opacity",
                crud.draft.restart_policy?.enabled ? "" : "opacity-50",
              ].join(" ")}
              aria-disabled={!crud.draft.restart_policy?.enabled}
            >
              <Field label="Max attempts">
                <input
                  type="number"
                  min={0}
                  disabled={!crud.draft.restart_policy?.enabled}
                  value={String(crud.draft.restart_policy?.max_attempts ?? "")}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              max_attempts:
                                event.target.value === ""
                                  ? undefined
                                  : Number(event.target.value),
                            },
                          }
                        : current,
                    )
                  }
                  className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
                />
              </Field>
              <Field label="Delay (ms)">
                <input
                  type="number"
                  min={0}
                  disabled={!crud.draft.restart_policy?.enabled}
                  value={Number(crud.draft.restart_policy?.delay_ms ?? 1000)}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              delay_ms: Number(event.target.value) || 0,
                            },
                          }
                        : current,
                    )
                  }
                  className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
                />
              </Field>
              <Field label="Backoff multiplier">
                <input
                  type="number"
                  min={1}
                  step="0.1"
                  disabled={!crud.draft.restart_policy?.enabled}
                  value={Number(crud.draft.restart_policy?.backoff_multiplier ?? 2)}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              backoff_multiplier:
                                Number(event.target.value) || 1,
                            },
                          }
                        : current,
                    )
                  }
                  className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
                />
              </Field>
              <Field label="Max delay (ms)">
                <input
                  type="number"
                  min={0}
                  disabled={!crud.draft.restart_policy?.enabled}
                  value={Number(crud.draft.restart_policy?.max_delay_ms ?? 30000)}
                  onChange={(event) =>
                    crud.setDraft((current) =>
                      current
                        ? {
                            ...current,
                            restart_policy: {
                              ...DEFAULT_RESTART_POLICY,
                              ...(current.restart_policy ?? {}),
                              max_delay_ms: Number(event.target.value) || 0,
                            },
                          }
                        : current,
                    )
                  }
                  className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
                />
              </Field>
            </div>

            <RestartScheduleHint policy={crud.draft.restart_policy} />
          </section>

          {crud.isEditingExisting && crud.draft ? (
            <LiveStatusSection
              draft={crud.draft}
              status={statuses[crud.draft.id]}
              restarting={restarting}
              onRestart={() => void handleRestart(crud.draft!.id)}
            />
          ) : null}

          <div className="mt-5 flex gap-3">
            <button
              type="button"
              onClick={() => void crud.handleSave()}
              disabled={crud.saving}
              className="rounded-xl bg-fg-strong px-4 py-2 text-sm font-medium text-bg transition hover:bg-fg disabled:cursor-not-allowed disabled:opacity-60"
            >
              {crud.saving ? "Saving..." : "Save"}
            </button>
            <button
              type="button"
              onClick={crud.cancelEdit}
              className="rounded-xl border border-line-strong px-4 py-2 text-sm font-medium text-fg transition hover:bg-soft"
            >
              Cancel
            </button>
          </div>
        </section>
      ) : null}

      <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
        <ListSearchBar
          value={search}
          onChange={(next) => applyListState({ search: next, page: 1 })}
          placeholder="Search by id, transport, endpoint, env key…"
        />
        <PageSizeSelect
          value={pageSize}
          onChange={(next) => applyListState({ pageSize: next, page: 1 })}
        />
      </div>

      <div className="overflow-x-auto rounded-md border border-line bg-surface shadow-card">
        {!crud.loading && crud.items.length === 0 ? (
          <EmptyState
            title="No managed MCP servers yet"
            description="MCP servers expose external tools (databases, APIs, files) to your agents. Configure transport, environment, and restart policy here."
            actions={
              <button
                type="button"
                onClick={startCreate}
                className="inline-flex h-9 items-center rounded-md bg-fg-strong px-4 text-sm font-medium text-bg transition-colors hover:bg-fg"
              >
                + New MCP Server
              </button>
            }
          />
        ) : (
          <>
            <table className="min-w-full">
              <SortableHeader
                columns={COLUMNS}
                sort={sort}
                onSort={(key) =>
                  applyListState({ sort: toggleSort(sort, key), page: 1 })
                }
              />
              <tbody>
                {crud.loading && <SkeletonRows rows={3} cols={COLUMNS.length} />}
                {!crud.loading && view.items.length === 0 && (
                  <tr>
                    <td colSpan={COLUMNS.length} className="px-5 py-8 text-center text-sm text-fg-soft">
                      No MCP servers match the current filter.
                    </td>
                  </tr>
                )}
                {!crud.loading && view.items.map((server) => (
                  <tr
                    key={server.id}
                    className="border-t border-line text-sm text-fg"
                  >
                    <td className="px-5 py-4 font-mono text-fg-strong">{server.id}</td>
                    <td className="px-5 py-4">{server.transport}</td>
                    <td className="px-5 py-4 text-fg-soft">
                      {endpointFor(server) || "Unconfigured"}
                    </td>
                    <td className="px-5 py-4 text-fg-soft">
                      {server.has_env
                        ? (server.env_keys ?? []).join(", ") || "Stored"
                        : "None"}
                    </td>
                    <td className="px-5 py-4 text-fg-soft">
                      {formatRelativeTime(server.updated_at)}
                    </td>
                    <td className="px-5 py-4">
                      <StatusBadge status={statuses[server.id]} />
                    </td>
                    <td className="px-5 py-4">
                      <div className="flex gap-4">
                        <button
                          type="button"
                          onClick={() => startEdit(server)}
                          className="font-medium text-fg transition hover:text-fg-strong"
                        >
                          Edit
                        </button>
                        <button
                          type="button"
                          onClick={() => void crud.handleDelete(server.id)}
                          className="font-medium text-tone-error transition hover:text-tone-error"
                        >
                          Delete
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {view.pageCount > 1 || view.totalItems > pageSize ? (
              <Pagination
                page={view.page}
                pageCount={view.pageCount}
                startIndex={view.startIndex}
                endIndex={view.endIndex}
                totalItems={view.totalItems}
                onPageChange={(p) => applyListState({ page: p })}
              />
            ) : null}
          </>
        )}
      </div>
    </div>
  );
}


function LiveStatusSection({
  draft,
  status,
  restarting,
  onRestart,
}: {
  draft: McpServerRecord;
  status: McpServerStatus | null | undefined;
  restarting: boolean;
  onRestart: () => void;
}) {
  const stateLabel =
    status === undefined
      ? "Loading…"
      : status === null
        ? "Unavailable"
        : status.connected
          ? "Connected"
          : "Disconnected";
  const stateTone: "success" | "neutral" | "error" =
    status === undefined || status === null
      ? "neutral"
      : status.connected
        ? "success"
        : "error";
  const handshake = status === undefined || status === null
    ? "—"
    : status.connected
      ? "ok"
      : "—";
  const toolCount = status?.tools?.length ?? 0;
  const restartHint = draft.restart_policy?.enabled
    ? `auto · max ${draft.restart_policy?.max_attempts ?? "∞"}`
    : "manual only";

  return (
    <section className="mt-5 rounded-md border border-line bg-soft p-4">
      <div className="flex items-center justify-between gap-4">
        <div className="flex items-center gap-2">
          <StatusBadge status={status} />
          <h4 className="text-sm font-semibold text-fg-strong">Live Status</h4>
        </div>
        <button
          type="button"
          disabled={restarting}
          onClick={onRestart}
          className="rounded-md border border-line-strong px-3 py-1.5 text-xs font-medium text-fg transition-colors hover:bg-muted disabled:cursor-not-allowed disabled:opacity-60"
        >
          {restarting ? "Restarting…" : "Restart"}
        </button>
      </div>

      <div className="mt-3 grid gap-3 md:grid-cols-2 xl:grid-cols-4">
        <StatusStat label="State" value={stateLabel} tone={stateTone} />
        <StatusStat label="Handshake" value={handshake} mono />
        <StatusStat label="Tools" value={String(toolCount)} mono />
        <StatusStat
          label={status && status.consecutive_failures > 0 ? "Failures (since last ok)" : "Restart"}
          value={
            status && status.consecutive_failures > 0
              ? `${status.consecutive_failures}${status.permanently_failed ? " · gave up" : status.reconnecting ? " · retrying" : ""}`
              : restartHint
          }
          tone={
            status && status.permanently_failed
              ? "error"
              : status && status.consecutive_failures > 0
                ? "warn"
                : "neutral"
          }
        />
      </div>

      {status && (status.last_attempt_at || status.last_success_at) && (
        <div className="mt-2 flex flex-wrap gap-x-4 text-[11px] text-fg-faint">
          {status.last_attempt_at && (
            <span>
              last attempt {formatRelativeTime(status.last_attempt_at)}
            </span>
          )}
          {status.last_success_at && (
            <span>
              last success {formatRelativeTime(status.last_success_at)}
            </span>
          )}
        </div>
      )}

      {status?.last_error && (
        <p className="mt-3 rounded-md border border-tone-error/30 bg-tone-error/10 px-3 py-2 font-mono text-xs text-tone-error">
          {status.last_error}
        </p>
      )}

      {toolCount > 0 && status?.tools && (
        <div className="mt-4 overflow-hidden rounded-md border border-line bg-surface">
          <p className="border-b border-line bg-soft px-3 py-2 text-[11px] font-medium uppercase tracking-[0.18em] text-fg-faint">
            Exposed tools ({toolCount})
          </p>
          <table className="w-full">
            <tbody>
              {status.tools.map((tool) => (
                <tr key={tool.name} className="border-t border-line text-sm first:border-t-0">
                  <td className="w-1/3 px-3 py-2 font-mono text-xs text-fg-strong">{tool.name}</td>
                  <td className="px-3 py-2 text-xs text-fg-soft">
                    {tool.description ?? "—"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function StatusStat({
  label,
  value,
  tone = "neutral",
  mono = false,
}: {
  label: string;
  value: string;
  tone?: "success" | "neutral" | "error" | "warn";
  mono?: boolean;
}) {
  const valueClass = [
    "mt-1 text-sm font-semibold",
    mono ? "font-mono" : "",
    tone === "success" ? "text-tone-success" : tone === "error" ? "text-tone-error" : tone === "warn" ? "text-tone-warn" : "text-fg-strong",
  ]
    .join(" ")
    .trim();
  return (
    <div className="rounded-md border border-line bg-surface px-3 py-2">
      <div className="text-[10px] font-medium uppercase tracking-[0.18em] text-fg-faint">{label}</div>
      <div className={valueClass}>{value}</div>
    </div>
  );
}

function RestartScheduleHint({
  policy,
}: {
  policy: McpRestartPolicy | undefined;
}) {
  if (!policy?.enabled) {
    return (
      <p className="mt-3 text-xs text-fg-faint">
        Auto-restart is off. The server stays down on crash and waits for a
        manual restart.
      </p>
    );
  }
  const initial = Math.max(0, Number(policy.delay_ms ?? 0));
  const multiplier = Math.max(1, Number(policy.backoff_multiplier ?? 1));
  const cap = Math.max(initial, Number(policy.max_delay_ms ?? initial));
  const max = Math.max(0, Number(policy.max_attempts ?? 0));
  const slots = max > 0 ? Math.min(max, 5) : 5;
  const schedule: string[] = [];
  let cur = initial;
  for (let i = 0; i < slots; i++) {
    schedule.push(formatBackoffMs(cur));
    cur = Math.min(cap, cur * multiplier);
  }
  return (
    <p className="mt-3 text-xs text-fg-soft">
      <span className="font-medium text-fg">Computed schedule:</span>{" "}
      {schedule.map((s, i) => (
        <span key={i}>
          {i > 0 && <span className="text-fg-faint"> → </span>}
          <span className="font-mono text-fg-strong">{s}</span>
        </span>
      ))}
      {max > slots && (
        <span className="ml-1 text-fg-faint">
          (… capped at {formatBackoffMs(cap)})
        </span>
      )}
      <span className="ml-1 text-fg-faint">
        {max > 0
          ? `· gives up after attempt ${max}`
          : `· retries forever (no max attempts)`}
      </span>
    </p>
  );
}

function formatBackoffMs(ms: number): string {
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60_000) {
    const s = ms / 1000;
    return Number.isInteger(s) ? `${s}s` : `${s.toFixed(1)}s`;
  }
  return `${(ms / 60_000).toFixed(1)}m`;
}
