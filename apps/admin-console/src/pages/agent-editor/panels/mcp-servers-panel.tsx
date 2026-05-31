import { Link } from "react-router";
import { type AgentSpec, type McpServerRecord } from "@/lib/config-api";
import { mcpServerPattern, selectedMcpServerIds } from "@/lib/agent-resource-references";
import { useMcpStatusQueries } from "@/lib/query/hooks/mcp";
import { adminRoutes } from "@/lib/routes";
import { safeErrorMessage } from "@/lib/safe-error-message";

export function McpServersPanel({
  spec,
  servers,
  loading,
  error,
  updateField,
}: {
  spec: AgentSpec;
  servers: McpServerRecord[] | null;
  loading: boolean;
  error: string | null;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
}) {
  const records = servers ?? [];
  const serverIds = records.map((server) => server.id);
  const statusQueries = useMcpStatusQueries(serverIds);
  const statusById = new Map(serverIds.map((id, index) => [id, statusQueries[index]] as const));

  if (loading && !servers) {
    return (
      <div className="rounded-sm border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        Loading MCP servers...
      </div>
    );
  }
  if (error) {
    return (
      <div className="rounded-sm border border-tone-error/30 bg-tone-error/10 p-4 text-sm text-tone-error">
        MCP servers unavailable: {error}
      </div>
    );
  }

  const selected = selectedMcpServerIds(
    spec,
    records.map((server) => server.id),
  );
  const selectedSet = new Set(selected);
  const universalAllowed = (spec.allowed_tool_patterns ?? []).includes("*");

  function toggleServer(serverId: string, checked: boolean) {
    const patterns = spec.allowed_tool_patterns ?? [];
    const pattern = mcpServerPattern(serverId);
    const next = checked
      ? Array.from(new Set([...patterns, pattern]))
      : patterns.filter((value) => value !== pattern);
    updateField("allowed_tool_patterns", next);
  }

  function removeUniversalAllow() {
    updateField(
      "allowed_tool_patterns",
      (spec.allowed_tool_patterns ?? []).filter((pattern) => pattern !== "*"),
    );
  }

  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">MCP Server Access</h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            Choose which MCP servers contribute tools to this agent. Selections are stored as tool
            allow patterns such as <span className="font-mono">mcp__server__*</span>.
          </p>
        </div>
        <div className="rounded-sm border border-line bg-soft px-3 py-2 text-right">
          <div className="font-mono text-lg font-semibold text-fg-strong">{selected.length}</div>
          <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
            servers selected
          </div>
        </div>
      </div>

      {universalAllowed ? (
        <div className="mt-4 flex flex-wrap items-center gap-3 rounded-sm border border-tone-warn/35 bg-tone-warn/10 px-4 py-3 text-sm text-tone-warn">
          <div className="min-w-0 flex-1">
            <span className="font-mono">*</span> is still allowed, so every published tool remains
            reachable. Remove it to make server-specific MCP selection restrictive.
          </div>
          <button
            type="button"
            onClick={removeUniversalAllow}
            className="rounded-sm border border-tone-warn/40 bg-surface px-2 py-1 text-xs font-medium text-tone-warn hover:bg-soft"
          >
            Remove *
          </button>
        </div>
      ) : null}

      {records.length === 0 ? (
        <div className="mt-4 flex flex-wrap items-center gap-3 rounded-sm border border-dashed border-line bg-soft px-4 py-3 text-sm text-fg-soft">
          <div className="min-w-0 flex-1">No MCP servers are registered yet.</div>
          <Link
            to={adminRoutes.mcpServers}
            className="rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs font-medium text-fg-soft hover:bg-muted hover:text-fg"
          >
            Open MCP Servers
          </Link>
        </div>
      ) : (
        <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
          {records.map((server) => {
            const checked = selectedSet.has(server.id);
            const statusQuery = statusById.get(server.id);
            const status = statusQuery?.data ?? null;
            const statusError = statusQuery?.error;
            const verifiedTools = status?.tools ?? [];
            const toolCount = verifiedTools.length;
            return (
              <label
                key={server.id}
                className={[
                  "rounded-sm border px-4 py-3 text-sm transition-colors",
                  checked
                    ? "border-agent-stripe/40 bg-agent-tint text-agent-fg"
                    : "border-line bg-soft text-fg hover:border-line-strong",
                ].join(" ")}
              >
                <div className="flex items-start gap-3">
                  <input
                    type="checkbox"
                    checked={checked}
                    onChange={(event) => toggleServer(server.id, event.target.checked)}
                    aria-label={`MCP server ${server.id}`}
                  />
                  <div className="min-w-0">
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-mono text-xs text-fg-strong">{server.id}</span>
                      <span className="rounded-pill bg-muted px-2 py-0.5 text-[10px] font-medium text-fg-soft">
                        {server.transport}
                      </span>
                    </div>
                    <div className="mt-1 truncate text-xs text-fg-soft">
                      {server.transport === "stdio"
                        ? [server.command, ...(server.args ?? [])].filter(Boolean).join(" ")
                        : server.url}
                    </div>
                    <div className="mt-2 font-mono text-[11px] text-fg-faint">
                      {mcpServerPattern(server.id)}
                    </div>
                    <div className="mt-3 flex flex-wrap items-center gap-2 text-[11px] text-fg-soft">
                      <span
                        className={[
                          "inline-flex items-center gap-1 rounded-pill px-2 py-0.5",
                          statusError
                            ? "bg-tone-error/10 text-tone-error"
                            : status?.connected
                              ? "bg-tone-success/15 text-tone-success"
                              : "bg-muted text-fg-soft",
                        ].join(" ")}
                      >
                        {statusQuery?.isFetching
                          ? "verifying"
                          : statusError
                            ? "verify failed"
                            : status?.connected
                              ? `${toolCount} tools`
                              : "not verified"}
                      </span>
                      <button
                        type="button"
                        onClick={(event) => {
                          event.preventDefault();
                          event.stopPropagation();
                          void statusQuery?.refetch();
                        }}
                        className="rounded-sm border border-line bg-surface px-2 py-0.5 text-[11px] font-medium text-fg-soft hover:bg-soft hover:text-fg"
                      >
                        Verify tools
                      </button>
                    </div>
                    {statusError ? (
                      <div className="mt-2 break-words font-mono text-[11px] text-tone-error">
                        {safeErrorMessage(statusError)}
                      </div>
                    ) : toolCount > 0 ? (
                      <details className="mt-2 rounded-sm border border-line bg-surface">
                        <summary className="cursor-pointer px-2 py-1 text-[11px] text-fg-soft">
                          Show tools
                        </summary>
                        <ul className="space-y-1 border-t border-line px-2 py-2">
                          {verifiedTools.map((tool) => (
                            <li key={tool.name} className="min-w-0">
                              <span className="font-mono text-[11px] text-fg-strong">
                                {tool.name}
                              </span>
                              {tool.description ? (
                                <span className="ml-2 text-[11px] text-fg-soft">
                                  {tool.description}
                                </span>
                              ) : null}
                            </li>
                          ))}
                        </ul>
                      </details>
                    ) : null}
                  </div>
                </div>
              </label>
            );
          })}
        </div>
      )}
    </section>
  );
}
