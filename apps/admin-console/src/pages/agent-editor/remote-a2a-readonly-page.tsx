import { Link } from "react-router";

import type { AgentSpec, ConfigSourceState } from "@/lib/config-api";
import { adminRoutes } from "@/lib/routes";
import { useA2aStatusQuery } from "@/lib/query/hooks/a2a";
import { safeErrorMessage } from "@/lib/safe-error-message";
import { RemoteEndpointReadonlySection } from "./remote-endpoint-readonly-section";

export function RemoteA2aAgentReadOnlyPage({
  spec,
  sourceState,
  a2aServerId,
  agentMetaError,
}: {
  spec: AgentSpec;
  sourceState: ConfigSourceState | null;
  a2aServerId: string | null;
  agentMetaError: string | null;
}) {
  const statusQuery = useA2aStatusQuery(a2aServerId ?? undefined);
  const card = statusQuery.data?.card ?? null;
  const skills = card?.skills ?? [];
  const interfaces = card?.supportedInterfaces ?? [];
  return (
    <div className="mx-auto w-full max-w-5xl p-6 md:p-8 2xl:max-w-none">
      <div className="mb-3 text-xs">
        <Link to={adminRoutes.agents} className="text-fg-soft hover:text-fg">
          ← Agents
        </Link>
      </div>
      <header className="mb-4 flex flex-wrap items-start justify-between gap-4">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="font-mono text-2xl font-semibold text-fg-strong">{spec.id}</h1>
            <span className="rounded bg-soft px-1.5 font-mono text-[10px] uppercase text-fg-soft">
              A2A
            </span>
            {sourceState ? <SourceBadge state={sourceState} /> : null}
          </div>
          <p className="mt-2 max-w-3xl text-sm text-fg-soft">
            This agent is discovered from an A2A server. Its model, prompt, tools, and runtime are
            owned by the remote server.
          </p>
        </div>
        {a2aServerId ? (
          <Link
            to={adminRoutes.a2aServers}
            className="inline-flex h-9 items-center rounded-sm border border-line-strong px-3 text-sm font-medium text-fg transition hover:bg-soft"
          >
            Open A2A servers
          </Link>
        ) : null}
      </header>

      {agentMetaError ? (
        <div className="mb-4 rounded-sm border border-tone-error/30 bg-tone-error/10 px-4 py-3 text-sm text-tone-error">
          Agent metadata unavailable: {agentMetaError}
        </div>
      ) : null}

      <section className="rounded-sm border border-line bg-surface p-5 shadow-card">
        <h2 className="text-sm font-semibold text-fg-strong">Remote source</h2>
        <dl className="mt-3 grid gap-3 md:grid-cols-2">
          <ReadOnlyField label="A2A server" value={a2aServerId ?? spec.registry ?? "Unknown"} />
          <ReadOnlyField label="Registry" value={spec.registry ?? "A2A discovery"} />
          <ReadOnlyField label="Endpoint" value={spec.endpoint?.base_url ?? "Unknown"} />
          <ReadOnlyField label="Target" value={spec.endpoint?.target ?? spec.id} />
        </dl>
      </section>

      {spec.endpoint ? (
        <div className="mt-4">
          <RemoteEndpointReadonlySection endpoint={spec.endpoint} />
        </div>
      ) : null}

      <section className="mt-4 rounded-sm border border-line bg-surface p-5 shadow-card">
        <div className="flex items-baseline justify-between">
          <h2 className="text-sm font-semibold text-fg-strong">A2A card</h2>
          <span className="text-xs text-fg-faint">
            {statusQuery.isPending
              ? "loading"
              : statusQuery.data?.connected
                ? "connected"
                : "unavailable"}
          </span>
        </div>
        {statusQuery.isError ? (
          <p className="mt-2 text-sm text-tone-error">{safeErrorMessage(statusQuery.error)}</p>
        ) : statusQuery.data && !statusQuery.data.connected ? (
          <p className="mt-2 text-sm text-tone-error">{statusQuery.data.last_error}</p>
        ) : card ? (
          <div className="mt-3">
            <div className="text-base font-semibold text-fg-strong">{card.name}</div>
            <p className="mt-1 text-sm text-fg-soft">{card.description}</p>
            <div className="mt-4 grid gap-4 md:grid-cols-2">
              <ReadonlyList
                title="Interfaces"
                empty="No interfaces advertised"
                items={interfaces.map((entry) => ({
                  key: `${entry.protocolBinding}:${entry.url}`,
                  title: `${entry.protocolBinding} ${entry.protocolVersion}`,
                  body: entry.agentId ? `${entry.url} · ${entry.agentId}` : entry.url,
                }))}
              />
              <ReadonlyList
                title="Skills"
                empty="No skills advertised"
                items={skills.map((skill) => ({
                  key: skill.id,
                  title: skill.name,
                  body: skill.description ?? skill.id,
                }))}
              />
            </div>
          </div>
        ) : (
          <p className="mt-2 text-sm text-fg-soft">
            {a2aServerId ? "Loading A2A card..." : "No A2A server id is attached to this agent."}
          </p>
        )}
      </section>
    </div>
  );
}

function SourceBadge({ state }: { state: ConfigSourceState }) {
  if (state === "builtin") {
    return (
      <span className="rounded-full bg-muted px-2 py-0.5 text-xs font-medium text-fg-soft">
        Built-in
      </span>
    );
  }
  if (state === "customized") {
    return (
      <span className="inline-flex items-center gap-1 rounded-full bg-blue-100 px-2 py-0.5 text-xs font-medium text-blue-800 dark:bg-blue-900/30 dark:text-blue-300">
        <span aria-hidden className="h-1.5 w-1.5 rounded-full bg-blue-500" />
        Customized
      </span>
    );
  }
  return (
    <span className="rounded-full bg-soft px-2 py-0.5 text-xs font-medium text-fg">
      User-defined
    </span>
  );
}

function ReadOnlyField({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <dt className="text-[10px] font-semibold uppercase tracking-eyebrow text-fg-faint">
        {label}
      </dt>
      <dd className="mt-1 break-all font-mono text-sm text-fg-strong">{value}</dd>
    </div>
  );
}

function ReadonlyList({
  title,
  empty,
  items,
}: {
  title: string;
  empty: string;
  items: Array<{ key: string; title: string; body: string }>;
}) {
  return (
    <div>
      <h3 className="text-xs font-semibold uppercase tracking-eyebrow text-fg-soft">{title}</h3>
      {items.length === 0 ? (
        <p className="mt-2 text-sm text-fg-faint">{empty}</p>
      ) : (
        <ul className="mt-2 space-y-2">
          {items.map((item) => (
            <li key={item.key} className="rounded-sm border border-line bg-soft px-3 py-2">
              <div className="text-sm font-medium text-fg">{item.title}</div>
              <div className="mt-0.5 break-all text-xs text-fg-soft">{item.body}</div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
