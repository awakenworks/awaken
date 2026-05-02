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

const SORT_CONFIG: SortConfig<AgentSpec, AgentSortKey> = {
  id: (a, b) => compareString(a.id, b.id),
  model_id: (a, b) => compareString(a.model_id, b.model_id),
  plugin_count: (a, b) =>
    compareNumber(a.plugin_ids?.length ?? 0, b.plugin_ids?.length ?? 0),
  updated_at: (a, b) => compareNumber(a.updated_at ?? 0, b.updated_at ?? 0),
};

const COLUMNS: SortableColumn<AgentSortKey>[] = [
  { key: "id", label: "ID" },
  { key: "model_id", label: "Model" },
  { key: "plugin_count", label: "Plugins" },
  { key: "updated_at", label: "Last modified" },
  { key: null, label: "Actions" },
];

const LIST_OPTIONS = {
  validSortKeys: ["id", "model_id", "plugin_count", "updated_at"] as const,
  defaultSort: { key: "id" as AgentSortKey, direction: "asc" as const },
} as const;

export function AgentsPage() {
  const navigate = useNavigate();
  const toast = useToast();
  const confirmDialog = useConfirmDialog();
  const [agents, setAgents] = useState<AgentSpec[]>([]);
  const [loading, setLoading] = useState(true);

  const { search, sort, pageSize, page, apply: applyListState } = useListUrlState<AgentSortKey>(LIST_OPTIONS);

  useEffect(() => {
    let cancelled = false;

    async function load() {
      setLoading(true);
      try {
        const response = await configApi.list<AgentSpec>("agents");
        if (!cancelled) {
          setAgents(response.items);
        }
      } catch (loadError) {
        if (!cancelled) {
          toast.error(
            loadError instanceof Error ? loadError.message : String(loadError),
          );
          setAgents([]);
        }
      } finally {
        if (!cancelled) {
          setLoading(false);
        }
      }
    }

    void load();

    return () => {
      cancelled = true;
    };
  }, [toast]);

  const filtered = useMemo(
    () =>
      filterBySearch(agents, search, (agent) => [
        agent.id,
        agent.model_id,
        ...(agent.plugin_ids ?? []),
      ]),
    [agents, search],
  );

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
    if (view.page !== page) {
      applyListState({ page: view.page });
    }
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
    if (!accepted) {
      return;
    }

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

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="mb-6 flex items-center justify-between gap-4">
        <div>
          <p className="text-sm font-medium uppercase tracking-[0.2em] text-slate-500">
            Runtime Catalog
          </p>
          <h2 className="mt-2 text-3xl font-semibold text-slate-950">Agents</h2>
        </div>
        <Link
          to={adminRoutes.agentNew}
          className="rounded-xl bg-slate-950 px-4 py-2 text-sm font-medium text-white transition hover:bg-slate-800"
        >
          New Agent
        </Link>
      </div>

      <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
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

      <div className="overflow-hidden rounded-2xl border border-slate-200 bg-white shadow-sm">
        {loading ? (
          <div className="px-5 py-6 text-sm text-slate-500">Loading agents...</div>
        ) : agents.length === 0 ? (
          <div className="px-5 py-6 text-sm text-slate-500">
            No managed agents yet.
          </div>
        ) : view.items.length === 0 ? (
          <div className="px-5 py-6 text-sm text-slate-500">
            No agents match the current filter.
          </div>
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
                {view.items.map((agent) => (
                  <tr
                    key={agent.id}
                    className="cursor-pointer border-t border-slate-200 text-sm text-slate-700 transition hover:bg-slate-50"
                    onClick={() => navigate(adminRoutes.agent(agent.id))}
                  >
                    <td className="px-5 py-4 font-mono text-slate-950">{agent.id}</td>
                    <td className="px-5 py-4">{agent.model_id}</td>
                    <td className="px-5 py-4 text-slate-500">
                      {(agent.plugin_ids ?? []).join(", ") || "None"}
                    </td>
                    <td className="px-5 py-4 text-slate-500">
                      {formatRelativeTime(agent.updated_at)}
                    </td>
                    <td className="px-5 py-4">
                      <button
                        type="button"
                        onClick={(event) => {
                          event.stopPropagation();
                          void handleDelete(agent.id);
                        }}
                        className="text-sm font-medium text-rose-600 transition hover:text-rose-700"
                      >
                        Delete
                      </button>
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
