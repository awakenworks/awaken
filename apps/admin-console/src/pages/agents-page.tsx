import { useEffect, useState } from "react";
import { Link, useNavigate } from "react-router";
import { type AgentSpec, configApi } from "@/lib/config-api";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { useToast } from "@/components/toast-provider";
import { adminRoutes } from "@/lib/routes";

export function AgentsPage() {
  const navigate = useNavigate();
  const toast = useToast();
  const confirmDialog = useConfirmDialog();
  const [agents, setAgents] = useState<AgentSpec[]>([]);
  const [loading, setLoading] = useState(true);

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

      <div className="overflow-hidden rounded-2xl border border-slate-200 bg-white shadow-sm">
        {loading ? (
          <div className="px-5 py-6 text-sm text-slate-500">Loading agents...</div>
        ) : agents.length === 0 ? (
          <div className="px-5 py-6 text-sm text-slate-500">
            No managed agents yet.
          </div>
        ) : (
          <table className="min-w-full">
            <thead className="bg-slate-50 text-left text-xs uppercase tracking-wide text-slate-500">
              <tr>
                <th className="px-5 py-3">ID</th>
                <th className="px-5 py-3">Model</th>
                <th className="px-5 py-3">Plugins</th>
                <th className="px-5 py-3">Actions</th>
              </tr>
            </thead>
            <tbody>
              {agents.map((agent) => (
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
        )}
      </div>
    </div>
  );
}
