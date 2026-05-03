import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link, useNavigate } from "react-router";
import { type AgentRuntimeSnapshot, type AgentSpec, configApi } from "@/lib/config-api";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { useToast } from "@/components/toast-provider";
import {
  ListSearchBar,
  PageSizeSelect,
  Pagination,
  SortableHeader,
  type SortableColumn,
} from "@/components/list-controls";
import { EmptyState } from "@/components/ui/empty-state";
import { FilterBar, FilterChip } from "@/components/ui/filter-bar";
import { PageHeader } from "@/components/ui/page-header";
import { Pill, PillStack } from "@/components/ui/pill";
import { SkeletonRows } from "@/components/ui/skeleton";
import { Sparkline } from "@/components/ui/sparkline";
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
import { adminRoutes } from "@/lib/routes";
import { formatRelativeTime } from "@/lib/format-time";

type AgentSortKey = "id" | "model_id" | "plugin_count" | "updated_at";

type ModifiedRange = "any" | "1h" | "24h" | "7d" | "30d";

const RANGE_OPTIONS: { value: ModifiedRange; label: string; seconds: number }[] = [
  { value: "any", label: "any", seconds: 0 },
  { value: "1h", label: "last 1h", seconds: 60 * 60 },
  { value: "24h", label: "last 24h", seconds: 60 * 60 * 24 },
  { value: "7d", label: "last 7d", seconds: 60 * 60 * 24 * 7 },
  { value: "30d", label: "last 30d", seconds: 60 * 60 * 24 * 30 },
];

const SORT_CONFIG: SortConfig<AgentSpec, AgentSortKey> = {
  id: (a, b) => compareString(a.id, b.id),
  model_id: (a, b) => compareString(a.model_id, b.model_id),
  plugin_count: (a, b) =>
    compareNumber(a.plugin_ids?.length ?? 0, b.plugin_ids?.length ?? 0),
  updated_at: (a, b) => compareNumber(a.updated_at ?? 0, b.updated_at ?? 0),
};

const COLUMNS: SortableColumn<AgentSortKey>[] = [
  { key: "id", label: "Agent" },
  { key: "model_id", label: "Model" },
  { key: "plugin_count", label: "Plugins" },
  { key: "updated_at", label: "Last modified" },
  { key: null, label: "Inferences (24h)" },
  { key: null, label: "Actions" },
];

const LIST_OPTIONS = {
  validSortKeys: ["id", "model_id", "plugin_count", "updated_at"] as const,
  defaultSort: { key: "updated_at" as AgentSortKey, direction: "desc" as const },
} as const;

export function AgentsPage() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const toast = useToast();
  const confirmDialog = useConfirmDialog();
  const [agents, setAgents] = useState<AgentSpec[]>([]);
  const [loading, setLoading] = useState(true);
  const [modelFilter, setModelFilter] = useState<string>("any");
  const [pluginFilter, setPluginFilter] = useState<string>("any");
  const [modifiedRange, setModifiedRange] = useState<ModifiedRange>("any");
  const [runtimeStats, setRuntimeStats] = useState<
    Map<string, AgentRuntimeSnapshot> | null
  >(null);
  const [runtimeUnavailable, setRuntimeUnavailable] = useState(false);

  const { search, sort, pageSize, page, apply: applyListState } =
    useListUrlState<AgentSortKey>(LIST_OPTIONS);

  useEffect(() => {
    let cancelled = false;
    async function load() {
      setLoading(true);
      try {
        const response = await configApi.list<AgentSpec>("agents");
        if (!cancelled) setAgents(response.items);
      } catch (loadError) {
        if (!cancelled) {
          toast.error(
            loadError instanceof Error ? loadError.message : String(loadError),
          );
          setAgents([]);
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    }
    void load();
    return () => {
      cancelled = true;
    };
  }, [toast]);

  useEffect(() => {
    let cancelled = false;
    void configApi
      .agentsRuntimeStats()
      .then((res) => {
        if (cancelled) return;
        if (!res) {
          setRuntimeStats(new Map());
          setRuntimeUnavailable(true);
          return;
        }
        const map = new Map<string, AgentRuntimeSnapshot>();
        for (const snap of res.agents) map.set(snap.agent_id, snap);
        setRuntimeStats(map);
        setRuntimeUnavailable(false);
      })
      .catch(() => {
        if (!cancelled) {
          setRuntimeStats(new Map());
          setRuntimeUnavailable(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const modelOptions = useMemo(() => {
    const set = new Set<string>();
    for (const a of agents) set.add(a.model_id);
    return [
      { value: "any", label: "any" },
      ...Array.from(set).sort().map((m) => ({ value: m, label: m })),
    ];
  }, [agents]);

  const pluginOptions = useMemo(() => {
    const set = new Set<string>();
    for (const a of agents) for (const p of a.plugin_ids ?? []) set.add(p);
    return [
      { value: "any", label: "any" },
      ...Array.from(set).sort().map((p) => ({ value: p, label: p })),
    ];
  }, [agents]);

  const filtered = useMemo(() => {
    const nowSec = Math.floor(Date.now() / 1000);
    const rangeSec =
      RANGE_OPTIONS.find((r) => r.value === modifiedRange)?.seconds ?? 0;
    return filterBySearch(agents, search, (agent) => [
      agent.id,
      agent.model_id,
      ...(agent.plugin_ids ?? []),
    ]).filter((a) => {
      if (modelFilter !== "any" && a.model_id !== modelFilter) return false;
      if (
        pluginFilter !== "any" &&
        !(a.plugin_ids ?? []).includes(pluginFilter)
      )
        return false;
      if (rangeSec > 0) {
        const updated = a.updated_at ?? 0;
        if (updated === 0 || nowSec - updated > rangeSec) return false;
      }
      return true;
    });
  }, [agents, search, modelFilter, pluginFilter, modifiedRange]);

  const sorted = useMemo(
    () => sortItems(filtered, sort, SORT_CONFIG),
    [filtered, sort],
  );

  const view = useMemo(
    () =>
      paginate(sorted, {
        page,
        pageSize,
        totalItems: sorted.length,
      }),
    [sorted, page, pageSize],
  );

  useEffect(() => {
    if (view.page !== page) applyListState({ page: view.page });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [view.page, page]);

  async function handleDelete(id: string) {
    const accepted = await confirmDialog({
      title: "Delete agent?",
      description: (
        <>
          This permanently removes <span className="font-mono">{id}</span> from
          the runtime catalog.
        </>
      ),
      confirmLabel: "Delete",
      tone: "destructive",
    });
    if (!accepted) return;
    try {
      await configApi.delete("agents", id);
      setAgents((current) => current.filter((agent) => agent.id !== id));
      toast.success(`Agent "${id}" deleted`);
    } catch (deleteError) {
      toast.error(
        deleteError instanceof Error ? deleteError.message : String(deleteError),
      );
    }
  }

  const sortLabelMap: Record<AgentSortKey, string> = {
    id: "id",
    model_id: "model",
    plugin_count: "plugins",
    updated_at: "last modified",
  };
  const sortMeta = sort ? (
    <Pill tone="info">
      {sortLabelMap[sort.key]} {sort.direction === "asc" ? "↑" : "↓"}
    </Pill>
  ) : null;

  const noAgentsAtAll = !loading && agents.length === 0;
  const noMatches = !loading && agents.length > 0 && view.items.length === 0;

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <PageHeader
        title={t("agents.title")}
        count={agents.length}
        actions={
          <Link
            to={adminRoutes.agentNew}
            className="inline-flex h-9 items-center rounded-md bg-accent px-3 text-sm font-medium text-accent-text transition-colors hover:opacity-90"
          >
            {t("agents.new")}
          </Link>
        }
      />

      <div className="mb-3 flex flex-wrap items-center gap-3">
        <ListSearchBar
          value={search}
          onChange={(next) => applyListState({ search: next, page: 1 })}
          placeholder={t("agents.searchPh")}
        />
        <PageSizeSelect
          value={pageSize}
          onChange={(next) => applyListState({ pageSize: next, page: 1 })}
        />
      </div>

      {!noAgentsAtAll && (
        <FilterBar
          filters={
            <>
              <FilterChip
                label="model"
                value={modelFilter}
                options={modelOptions}
                onChange={setModelFilter}
              />
              <FilterChip
                label="plugin"
                value={pluginFilter}
                options={pluginOptions}
                onChange={setPluginFilter}
              />
              <FilterChip
                label="modified"
                value={modifiedRange}
                options={RANGE_OPTIONS.map((r) => ({
                  value: r.value,
                  label: r.label,
                }))}
                onChange={setModifiedRange}
              />
            </>
          }
          sort={sortMeta}
          meta={`showing ${view.items.length} of ${agents.length}`}
        />
      )}

      {runtimeUnavailable && (
        <div className="mb-3 rounded-md border border-tone-warn/30 bg-tone-warn/10 px-3 py-2 text-xs text-fg-soft">
          <span className="font-medium text-fg-strong">Runtime stats disabled.</span>{" "}
          The "Inferences (24h)" column shows <span className="font-mono">n/a</span> because the server
          has no <span className="font-mono">RuntimeStatsRegistry</span> installed (install the
          observability plugin or wire <span className="font-mono">AppState::with_runtime_stats</span>).
        </div>
      )}

      <div className="overflow-hidden rounded-lg border border-line bg-surface shadow-card">
        {noAgentsAtAll ? (
          <EmptyState
            title={t("agents.empty.title")}
            description={t("agents.empty.desc")}
            actions={
              <Link
                to={adminRoutes.agentNew}
                className="inline-flex h-9 items-center rounded-md bg-accent px-4 text-sm font-medium text-accent-text transition-colors hover:opacity-90"
              >
                {t("agents.new")}
              </Link>
            }
          />
        ) : (
          <table className="min-w-full">
            <SortableHeader
              columns={COLUMNS}
              sort={sort}
              onSort={(key) =>
                applyListState({ sort: toggleSort(sort, key), page: 1 })
              }
            />
            <tbody>
              {loading && <SkeletonRows rows={4} cols={COLUMNS.length} />}
              {!loading && noMatches && (
                <tr>
                  <td colSpan={COLUMNS.length} className="px-5 py-8 text-center text-sm text-fg-soft">
                    {t("agents.noMatches")}
                  </td>
                </tr>
              )}
              {!loading &&
                view.items.map((agent) => (
                  <tr
                    key={agent.id}
                    className="cursor-pointer border-t border-line text-sm text-fg transition-colors hover:bg-soft"
                    onClick={() => navigate(adminRoutes.agent(agent.id))}
                  >
                    <td className="px-5 py-4">
                      <div className="font-medium text-fg-strong">{agent.id}</div>
                    </td>
                    <td className="px-5 py-4 font-mono text-fg">{agent.model_id}</td>
                    <td className="px-5 py-4">
                      <PillStack
                        items={agent.plugin_ids ?? []}
                        max={3}
                        empty="None"
                      />
                    </td>
                    <td className="px-5 py-4 text-fg-soft">
                      {formatRelativeTime(agent.updated_at)}
                    </td>
                    <td className="px-5 py-4 font-mono text-fg">
                      <InferenceCount
                        snapshot={runtimeStats?.get(agent.id)}
                        loading={runtimeStats === null}
                        unavailable={runtimeUnavailable}
                      />
                    </td>
                    <td className="px-5 py-4">
                      <div className="flex items-center gap-3">
                        <Link
                          to={adminRoutes.agentDashboard(agent.id)}
                          onClick={(e) => e.stopPropagation()}
                          className="text-xs font-medium text-link transition-colors hover:text-link-hover"
                        >
                          {t("agents.actions.dashboard")}
                        </Link>
                        <button
                          type="button"
                          onClick={(event) => {
                            event.stopPropagation();
                            void handleDelete(agent.id);
                          }}
                          className="text-xs font-medium text-tone-error transition-colors hover:underline"
                        >
                          {t("agents.actions.delete")}
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
            </tbody>
          </table>
        )}
        {!noAgentsAtAll && (view.pageCount > 1 || view.totalItems > pageSize) && (
          <Pagination
            page={view.page}
            pageCount={view.pageCount}
            startIndex={view.startIndex}
            endIndex={view.endIndex}
            totalItems={view.totalItems}
            onPageChange={(p) => applyListState({ page: p })}
          />
        )}
      </div>
    </div>
  );
}

function InferenceCount({
  snapshot,
  loading,
  unavailable,
}: {
  snapshot: AgentRuntimeSnapshot | undefined;
  loading: boolean;
  unavailable: boolean;
}) {
  if (loading) return <span className="text-fg-faint">…</span>;
  if (unavailable) {
    return (
      <span className="text-fg-faint" title="Runtime stats registry not configured on the server">
        n/a
      </span>
    );
  }
  if (!snapshot) return <span className="text-fg-faint">—</span>;
  const hasErrors = snapshot.error_count > 0;
  const hasInferences = snapshot.inference_count > 0;
  // Derive a tiny trend from the latency histogram bucket counts when present.
  // It's not the literal req-per-bucket but it gives a sense of distribution
  // shape without needing a per-time-series endpoint.
  const trend = (snapshot.inference_duration_histogram ?? [])
    .slice(0, 12)
    .map((b) => b.count);
  return (
    <div className="flex items-center gap-3">
      <div className="flex flex-col gap-0.5">
        <div className="inline-flex items-baseline gap-2">
          <span className="font-semibold text-fg-strong">
            {snapshot.inference_count}
          </span>
          {hasErrors && (
            <span
              className="text-xs text-tone-error"
              title={`${snapshot.error_count} error${snapshot.error_count === 1 ? "" : "s"}`}
            >
              · {snapshot.error_count} err
            </span>
          )}
        </div>
        {hasInferences && snapshot.p95_inference_duration_ms > 0 && (
          <div className="text-[11px] text-fg-faint">
            p95 {snapshot.p95_inference_duration_ms}ms
          </div>
        )}
      </div>
      {hasInferences && trend.length >= 2 && (
        <span className="text-fg-faint" aria-hidden>
          <Sparkline values={trend} ariaLabel={`latency distribution sparkline (${trend.length} buckets)`} />
        </span>
      )}
    </div>
  );
}
