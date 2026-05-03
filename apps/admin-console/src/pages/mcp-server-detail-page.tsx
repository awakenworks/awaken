import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Link, useNavigate, useParams } from "react-router";
import {
  ConfigApiError,
  type AgentSpec,
  type McpServerRecord,
  type McpServerStatusResponse,
  configApi,
} from "@/lib/config-api";
import { useToast } from "@/components/toast-provider";
import { useConfirmDialog } from "@/components/confirm-dialog";
import { Pill } from "@/components/ui/pill";
import { Sparkline } from "@/components/ui/sparkline";
import { adminRoutes } from "@/lib/routes";
import { formatRelativeTime } from "@/lib/format-time";

/**
 * MCP Server detail — drill-in for one server. Mirrors the design's
 * 4-stat live status card (PROCESS / HANDSHAKE / LAST CALL / ERRORS · 24h),
 * adds command preview + Copy, and shows the reverse "used by N agents"
 * lookup derived from agent specs that reference this server in their
 * sections.
 */
export function McpServerDetailPage() {
  const { t } = useTranslation();
  const { id } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const toast = useToast();
  const confirm = useConfirmDialog();
  const [server, setServer] = useState<McpServerRecord | null>(null);
  const [status, setStatus] = useState<McpServerStatusResponse | null | undefined>(undefined);
  const [agents, setAgents] = useState<AgentSpec[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notFound, setNotFound] = useState(false);
  const [restarting, setRestarting] = useState(false);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    setError(null);
    setNotFound(false);
    void Promise.all([
      configApi.get<McpServerRecord>("mcp-servers", id),
      configApi.mcpStatus(id).catch(() => null),
      configApi.list<AgentSpec>("agents").catch(() => ({ items: [] as AgentSpec[] })),
    ])
      .then(([rec, st, ag]) => {
        if (cancelled) return;
        setServer(rec);
        setStatus(st);
        setAgents(ag.items);
      })
      .catch((err) => {
        if (cancelled) return;
        if (err instanceof ConfigApiError && err.status === 404) {
          setNotFound(true);
          return;
        }
        setError(err instanceof Error ? err.message : String(err));
      });
    return () => {
      cancelled = true;
    };
  }, [id]);

  const usedByAgents = useMemo(() => {
    if (!id) return [] as AgentSpec[];
    return agents.filter((a) => mentionsMcp(a, id));
  }, [agents, id]);

  async function handleRestart() {
    if (!id) return;
    const ok = await confirm({
      title: `Restart "${id}"?`,
      description: "This reconnects the server. In-flight tool calls may be interrupted.",
      confirmLabel: "Restart",
    });
    if (!ok) return;
    setRestarting(true);
    try {
      await configApi.mcpRestart(id);
      toast.push({ message: `Restart triggered for "${id}".`, tone: "success" });
      const fresh = await configApi.mcpStatus(id).catch(() => null);
      setStatus(fresh);
    } catch (err) {
      toast.push({
        message: `Restart failed: ${err instanceof Error ? err.message : String(err)}`,
        tone: "error",
      });
    } finally {
      setRestarting(false);
    }
  }

  async function handleCopyCommand() {
    if (!server) return;
    const cmd = formatCommand(server);
    if (!cmd) return;
    try {
      await navigator.clipboard.writeText(cmd);
      setCopied(true);
      window.setTimeout(() => setCopied(false), 1200);
    } catch {
      // clipboard failures (iframe/non-https) are non-fatal
    }
  }

  if (!id) {
    return (
      <div className="mx-auto max-w-5xl p-6 md:p-8 text-sm text-fg-soft">
        Missing server id.
      </div>
    );
  }
  if (notFound) {
    return (
      <div className="mx-auto max-w-5xl p-6 md:p-8">
        <div className="mb-3 text-xs">
          <Link to={adminRoutes.mcpServers} className="text-fg-soft hover:text-fg">
            ← {t("mcp.title")}
          </Link>
        </div>
        <div className="rounded-md border border-line bg-surface p-6 text-sm text-fg-soft shadow-sm">
          <div className="text-fg-strong">MCP server &ldquo;{id}&rdquo; not found.</div>
          <div className="mt-1">It may have been removed, or the URL may be wrong.</div>
        </div>
      </div>
    );
  }
  if (error) {
    return (
      <div className="mx-auto max-w-5xl p-6 md:p-8">
        <div className="mb-3 text-xs">
          <Link to={adminRoutes.mcpServers} className="text-fg-soft hover:text-fg">
            ← {t("mcp.title")}
          </Link>
        </div>
        <div className="rounded-md border border-tone-error/30 bg-tone-error/10 p-4 text-sm text-tone-error">
          {error}
        </div>
      </div>
    );
  }
  if (!server) {
    return (
      <div className="mx-auto max-w-5xl p-6 md:p-8 text-sm text-fg-soft">
        {t("common.loading")}
      </div>
    );
  }

  const cmd = formatCommand(server);
  const isStdio = server.transport === "stdio";
  const isConnected = status?.connected ?? false;
  const lastAttempt = status?.last_attempt_at ? new Date(status.last_attempt_at * 1000) : null;
  const lastSuccess = status?.last_success_at ? new Date(status.last_success_at * 1000) : null;
  const failures = status?.consecutive_failures ?? 0;

  return (
    <div className="mx-auto max-w-5xl p-6 md:p-8">
      <header className="mb-4">
        <div className="mb-2 text-xs">
          <Link to={adminRoutes.mcpServers} className="text-fg-soft hover:text-fg">
            ← {t("mcp.title")}
          </Link>
        </div>
        <div className="flex items-baseline justify-between gap-4">
          <div className="flex items-baseline gap-3">
            <h2 className="text-2xl font-semibold tracking-title-em text-fg-strong">
              {server.id}
            </h2>
            <span className="rounded bg-soft px-1.5 font-mono text-[10px] text-fg-soft">
              {server.transport}
            </span>
          </div>
          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={handleRestart}
              disabled={restarting}
              className="inline-flex h-9 items-center rounded-md border border-line-strong bg-surface px-3 text-sm font-medium text-fg transition hover:bg-soft disabled:cursor-not-allowed disabled:opacity-60"
            >
              {restarting ? "↻ Restarting…" : "↻ Restart"}
            </button>
          </div>
        </div>
        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs">
          <Pill tone={isConnected ? "success" : status === null ? "neutral" : "error"}>
            {isConnected ? "● connected" : status === null ? "status unavailable" : "disconnected"}
          </Pill>
          <Pill tone="neutral">{(status?.tools ?? []).length} tools exposed</Pill>
          <Pill tone={usedByAgents.length > 0 ? "info" : "neutral"}>
            used by {usedByAgents.length} agent{usedByAgents.length === 1 ? "" : "s"}
          </Pill>
        </div>
      </header>

      {/* 4-stat live card */}
      <section className="overflow-hidden rounded-md border border-line bg-surface shadow-card">
        <div className="flex items-baseline justify-between border-b border-line px-5 py-3">
          <h3 className="text-sm font-semibold text-fg-strong">Live status</h3>
          <span className="text-xs text-fg-faint">refreshes on action</span>
        </div>
        <div className="grid grid-cols-1 gap-px bg-line sm:grid-cols-2 lg:grid-cols-4">
          <Cell
            label="HANDSHAKE"
            value={
              isConnected ? (
                <span className="flex items-center gap-1.5">
                  <span aria-hidden className="inline-block h-2 w-2 rounded-pill bg-state-done" />
                  ok
                </span>
              ) : status === null ? (
                <span className="text-fg-faint">—</span>
              ) : (
                <span className="text-tone-error">failed</span>
              )
            }
            sub={status?.tools.length ? `${status.tools.length} tools advertised` : "—"}
          />
          <Cell
            label="LAST ATTEMPT"
            value={lastAttempt ? formatRelativeTime(Math.floor(lastAttempt.getTime() / 1000)) : "—"}
            sub={lastAttempt ? lastAttempt.toLocaleString() : "no attempts yet"}
          />
          <Cell
            label="LAST SUCCESS"
            value={lastSuccess ? formatRelativeTime(Math.floor(lastSuccess.getTime() / 1000)) : "—"}
            sub={lastSuccess ? lastSuccess.toLocaleString() : "never succeeded"}
          />
          <Cell
            label="FAILURES (since last ok)"
            value={
              <span className={failures > 0 ? "text-tone-error" : "text-fg-strong"}>
                {failures}
              </span>
            }
            sub={
              status?.permanently_failed
                ? "gave up retrying"
                : status?.reconnecting
                  ? "retrying with backoff"
                  : failures === 0
                    ? "healthy"
                    : "transient"
            }
          />
        </div>
        {status?.last_error && (
          <div className="border-t border-line bg-tone-error/5 px-5 py-2 font-mono text-xs text-tone-error">
            last_error: {status.last_error}
          </div>
        )}
      </section>

      {/* Command preview */}
      {cmd && (
        <section className="mt-4 overflow-hidden rounded-md border border-line bg-code-bg shadow-card">
          <div className="flex items-center justify-between gap-2 px-4 py-2">
            <code className="break-all font-mono text-xs text-code-fg">
              {isStdio && <span className="text-code-fg/60">$ </span>}
              {cmd}
            </code>
            <button
              type="button"
              onClick={handleCopyCommand}
              className="shrink-0 rounded border border-code-fg/20 bg-code-fg/10 px-2 py-1 text-[10px] font-medium text-code-fg/85 hover:bg-code-fg/20"
            >
              {copied ? "✓ Copied" : "Copy"}
            </button>
          </div>
        </section>
      )}

      {/* Tools exposed */}
      <section className="mt-4 rounded-md border border-line bg-surface p-5 shadow-card">
        <div className="flex items-baseline justify-between">
          <h3 className="text-sm font-semibold text-fg-strong">Tools exposed</h3>
          <span className="font-mono text-xs text-fg-faint">
            {(status?.tools ?? []).length}
          </span>
        </div>
        {(status?.tools ?? []).length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">
            {status === null
              ? "status unavailable"
              : isConnected
                ? "no tools advertised"
                : "not connected"}
          </p>
        ) : (
          <ul className="mt-3 space-y-2">
            {(status?.tools ?? []).map((tool) => (
              <li
                key={tool.name}
                className="rounded-md border border-line bg-soft px-3 py-2"
              >
                <div className="font-mono text-sm text-fg-strong">{tool.name}</div>
                {tool.description && (
                  <div className="mt-0.5 text-xs text-fg-soft">{tool.description}</div>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* Reverse: used by */}
      <section className="mt-4 rounded-md border border-line bg-surface p-5 shadow-card">
        <h3 className="text-sm font-semibold text-fg-strong">Used by</h3>
        {usedByAgents.length === 0 ? (
          <p className="mt-2 text-sm text-fg-soft">No agents reference this server.</p>
        ) : (
          <ul className="mt-3 space-y-1.5">
            {usedByAgents.map((a) => (
              <li key={a.id} className="flex items-center justify-between gap-3 rounded-md border border-line bg-soft px-3 py-2">
                <Link to={adminRoutes.agent(a.id)} className="font-mono text-sm text-fg-strong hover:underline">
                  {a.id}
                </Link>
                <span className="font-mono text-xs text-fg-soft">{a.model_id}</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      <footer className="mt-6 flex items-center justify-between gap-3 text-xs text-fg-faint">
        <span>updated {formatRelativeTime(server.updated_at)}</span>
        <button
          type="button"
          onClick={() => navigate(adminRoutes.mcpServers)}
          className="text-fg-soft hover:text-fg-strong"
        >
          ← back to list
        </button>
      </footer>
    </div>
  );
}

function Cell({
  label,
  value,
  sub,
}: {
  label: string;
  value: React.ReactNode;
  sub?: React.ReactNode;
}) {
  return (
    <div className="bg-surface px-4 py-3">
      <div className="text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">{label}</div>
      <div className="mt-1 text-base font-semibold text-fg-strong">{value}</div>
      {sub !== undefined && <div className="mt-0.5 text-[11px] text-fg-faint">{sub}</div>}
    </div>
  );
}

function formatCommand(server: McpServerRecord): string {
  if (server.transport === "stdio") {
    return [server.command, ...(server.args ?? [])].filter(Boolean).join(" ");
  }
  return server.url ?? "";
}

/**
 * Heuristic: an agent "uses" an MCP server when its sections.mcp.servers
 * list (or its plugin_ids) references the server id. Different deployments
 * shape the section differently; we look in two common places.
 */
function mentionsMcp(agent: AgentSpec, mcpId: string): boolean {
  const sections = (agent as { sections?: Record<string, unknown> }).sections ?? {};
  const mcpSection = sections.mcp as { servers?: Array<{ id?: string } | string> } | undefined;
  if (mcpSection?.servers) {
    for (const s of mcpSection.servers) {
      if (typeof s === "string" && s === mcpId) return true;
      if (typeof s === "object" && s !== null && s.id === mcpId) return true;
    }
  }
  if ((agent.plugin_ids ?? []).includes(mcpId)) return true;
  return false;
}

// silence unused-import lints for the optional Sparkline (kept for a follow-up
// activations chart once backend exposes the time series)
void Sparkline;
