import { useEffect, useMemo, useState } from "react";
import { Link, useNavigate } from "react-router";
import { type AgentSpec, configApi } from "@/lib/config-api";
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
  { key: null, label: "Activity" },
  { key: null, label: "Actions" },
];

/** Stable per-id pseudo-activity sparkline samples until run history is wired
 *  to the runtime feed. Hashing the id keeps the silhouette consistent across
 *  re-renders so the table doesn't shimmer. */
function activitySamples(id: string): number[] {
  let h = 2166136261;
  for (let i = 0; i < id.length; i++) {
    h ^= id.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  const samples: number[] = [];
  for (let i = 0; i < 12; i++) {
    h = Math.imul(h ^ (h >>> 13), 1274126177);
    samples.push((h >>> 0) % 16);
  }
  return samples;
}

const LIST_OPTIONS = {
  validSortKeys: ["id", "model_id", "plugin_count", "updated_at"] as const,
  defaultSort: { key: "updated_at" as AgentSortKey, direction: "desc" as const },
} as const;

export function AgentsPage() {
  const navigate = useNavigate();
  const toast = useToast();
  const confirmDialog = useConfirmDialog();
  const [agents, setAgents] = useState<AgentSpec[]>([]);
  const [loading, setLoading] = useState(true);
  const [modelFilter, setModelFilter] = useState<string>("any");
  const [pluginFilter, setPluginFilter] = useState<string>("any");
  const [modifiedRange, setModifiedRange] = useState<ModifiedRange>("any");

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
  const sortMeta = (
    <Pill tone="info">
      {sortLabelMap[sort.key]} {sort.direction === "asc" ? "↑" : "↓"}
    </Pill>
  );

  const noAgentsAtAll = !loading && agents.length === 0;
  const noMatches = !loading && agents.length > 0 && view.items.length === 0;

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <PageHeader
        eyebrow="Configure"
        title="Agents"
        count={agents.length}
        description="Compose runtime-safe agent specs that pin a model, plugin set, and tool whitelist."
        actions={
          <Link
            to={adminRoutes.agentNew}
            className="inline-flex h-9 items-center rounded-md bg-fg-strong px-3 text-sm font-medium text-bg transition-colors hover:bg-fg"
          >
            + New Agent
          </Link>
        }
      />

      <div className="mb-3 flex flex-wrap items-center gap-3">
        <ListSearchBar
          value={search}
          onChange={(next) => applyListState({ search: next, page: 1 })}
          placeholder="Search by id, model, or plugin…"
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

      <div className="overflow-hidden rounded-lg border border-line bg-surface shadow-card">
        {noAgentsAtAll ? (
          <EmptyState
            title="No managed agents yet"
            description="Agents pair a model with a plugin recipe. Start by cloning the runtime defaults or scratch-building a new spec."
            actions={
              <Link
                to={adminRoutes.agentNew}
                className="inline-flex h-9 items-center rounded-md bg-fg-strong px-4 text-sm font-medium text-bg transition-colors hover:bg-fg"
              >
                + New Agent
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
                    No agents match the current filter.
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
                      <div className="font-mono text-xs text-fg-faint">
                        agt_{agent.id.slice(0, 8)}
                      </div>
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
                    <td className="px-5 py-4">
                      <Sparkline values={activitySamples(agent.id)} ariaLabel={`recent activity for ${agent.id}`} />
                    </td>
                    <td className="px-5 py-4">
                      <div className="flex items-center gap-3">
                        <Link
                          to={adminRoutes.agentDashboard(agent.id)}
                          onClick={(e) => e.stopPropagation()}
                          className="text-xs font-medium text-link transition-colors hover:text-link-hover"
                        >
                          ↗ Dashboard
                        </Link>
                        <button
                          type="button"
                          onClick={(event) => {
                            event.stopPropagation();
                            void handleDelete(agent.id);
                          }}
                          className="text-xs font-medium text-tone-error transition-colors hover:underline"
                        >
                          Delete
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
