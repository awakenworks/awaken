import { useEffect, useState } from "react";
import { Link } from "react-router";
import {
  type Capabilities,
  type McpServerRecord,
  configApi,
} from "@/lib/config-api";
import { adminRoutes } from "@/lib/routes";

type DashboardData = {
  capabilities: Capabilities;
  mcpServers: McpServerRecord[];
};

export function DashboardPage() {
  const [data, setData] = useState<DashboardData | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;

    async function load() {
      try {
        const [capabilities, mcpServers] = await Promise.all([
          configApi.capabilities(),
          configApi.list<McpServerRecord>("mcp-servers"),
        ]);

        if (!cancelled) {
          setData({
            capabilities,
            mcpServers: mcpServers.items,
          });
          setError(null);
        }
      } catch (loadError) {
        if (!cancelled) {
          setError(
            loadError instanceof Error ? loadError.message : String(loadError),
          );
        }
      }
    }

    void load();

    return () => {
      cancelled = true;
    };
  }, []);

  if (error) {
    return <PageError message={error} />;
  }

  if (!data) {
    return <PageLoading />;
  }

  const { capabilities, mcpServers } = data;

  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <header className="mb-8">
        <p className="text-sm font-medium uppercase tracking-[0.2em] text-slate-500">
          Live Registry
        </p>
        <h2 className="mt-2 text-3xl font-semibold text-slate-950">Dashboard</h2>
        <p className="mt-2 max-w-2xl text-sm text-slate-600">
          The counts below reflect the currently published runtime snapshot, not
          just what is stored on disk.
        </p>
      </header>

      <section className="grid gap-4 md:grid-cols-2 xl:grid-cols-3 2xl:grid-cols-6">
        <StatCard
          label="Agents"
          count={capabilities.agents.length}
          to={adminRoutes.agents}
        />
        <StatCard
          label="Skills"
          count={capabilities.skills.length}
          to={adminRoutes.skills}
        />
        <StatCard
          label="Models"
          count={capabilities.models.length}
          to={adminRoutes.models}
        />
        <StatCard
          label="Providers"
          count={capabilities.providers.length}
          to={adminRoutes.providers}
        />
        <StatCard
          label="MCP Servers"
          count={mcpServers.length}
          to={adminRoutes.mcpServers}
        />
        <StatCard label="Tools" count={capabilities.tools.length} />
      </section>

      <section className="mt-8 grid gap-6 lg:grid-cols-2">
        <div className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-slate-900">Plugins</h3>
            <span className="text-sm text-slate-500">
              {capabilities.plugins.length} registered
            </span>
          </div>
          {capabilities.plugins.length === 0 ? (
            <p className="mt-4 text-sm text-slate-500">No plugins registered.</p>
          ) : (
            <ul className="mt-4 space-y-3">
              {capabilities.plugins.map((plugin) => (
                <li
                  key={plugin.id}
                  className="rounded-xl border border-slate-200 bg-slate-50 px-4 py-3"
                >
                  <div className="font-mono text-sm text-slate-900">{plugin.id}</div>
                  <div className="mt-1 text-sm text-slate-500">
                    {plugin.config_schemas.length === 0
                      ? "No config sections"
                      : `Config sections: ${plugin.config_schemas
                          .map((schema) => schema.key)
                          .join(", ")}`}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>

        <div className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
          <div className="flex items-center justify-between">
            <h3 className="text-lg font-semibold text-slate-900">Published Tools</h3>
            <span className="text-sm text-slate-500">
              {capabilities.tools.length} live
            </span>
          </div>
          {capabilities.tools.length === 0 ? (
            <p className="mt-4 text-sm text-slate-500">No tools published.</p>
          ) : (
            <ul className="mt-4 max-h-[24rem] space-y-3 overflow-auto">
              {capabilities.tools.map((tool) => (
                <li
                  key={tool.id}
                  className="rounded-xl border border-slate-200 bg-slate-50 px-4 py-3"
                >
                  <div className="font-mono text-sm text-slate-900">{tool.id}</div>
                  <div className="mt-1 text-sm text-slate-500">
                    {tool.description || tool.name}
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
      </section>
    </div>
  );
}

function StatCard({
  label,
  count,
  to,
}: {
  label: string;
  count: number;
  to?: string;
}) {
  const card = (
    <div className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm transition hover:-translate-y-0.5 hover:shadow-md">
      <div className="text-3xl font-semibold text-slate-950">{count}</div>
      <div className="mt-2 text-sm text-slate-500">{label}</div>
    </div>
  );

  return to ? <Link to={to}>{card}</Link> : card;
}

function PageLoading() {
  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="rounded-2xl border border-slate-200 bg-white p-6 text-sm text-slate-500 shadow-sm">
        Loading dashboard...
      </div>
    </div>
  );
}

function PageError({ message }: { message: string }) {
  return (
    <div className="mx-auto max-w-6xl p-6 md:p-8">
      <div className="rounded-2xl border border-rose-200 bg-rose-50 p-6 text-sm text-rose-700 shadow-sm">
        {message}
      </div>
    </div>
  );
}
