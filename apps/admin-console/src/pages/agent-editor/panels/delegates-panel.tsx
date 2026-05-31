import { type AgentSpec, type Capabilities } from "@/lib/config-api";

export function DelegatesPanel({
  spec,
  capabilities,
  toggleDelegate,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  toggleDelegate: (delegateId: string, checked: boolean) => void;
}) {
  if (!capabilities || capabilities.agents.length === 0) {
    return (
      <div className="rounded-sm border border-dashed border-line bg-surface p-6 text-sm text-fg-soft">
        No other agents are registered yet, so this agent cannot delegate.
      </div>
    );
  }
  const selected = spec.delegates ?? [];
  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">Delegates (Sub-Agents)</h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            Pick agents this one can hand work off to. Self-loops are blocked statically; longer
            cycles (A → B → A) are detected at runtime by the scheduler.
          </p>
        </div>
        {selected.length > 0 && (
          <div className="flex flex-wrap items-center gap-1.5 text-xs text-fg-soft">
            <span className="text-fg-faint">selected</span>
            {selected.map((id) => (
              <span
                key={id}
                className="inline-flex items-center gap-1 rounded-pill border border-agent-stripe/30 bg-agent-tint px-2 py-0.5 font-mono text-agent-fg"
              >
                {id}
                <span className="font-sans text-[10px] uppercase tracking-eyebrow text-agent-fg/70">
                  Sub-Agent
                </span>
              </span>
            ))}
          </div>
        )}
      </div>
      <div className="mt-4 grid gap-3 md:grid-cols-2 xl:grid-cols-3">
        {capabilities.agents
          .filter((agentId) => agentId !== spec.id)
          .map((agentId) => {
            const checked = selected.includes(agentId);
            return (
              <label
                key={agentId}
                className={[
                  "rounded-sm border px-4 py-3 text-sm transition-colors",
                  checked
                    ? "border-agent-stripe/40 bg-agent-tint text-agent-fg"
                    : "border-line bg-soft text-fg hover:border-line-strong",
                ].join(" ")}
              >
                <div className="flex items-center gap-3">
                  <input
                    type="checkbox"
                    checked={checked}
                    aria-label={agentId}
                    onChange={(event) => toggleDelegate(agentId, event.target.checked)}
                  />
                  <span className="font-mono text-fg-strong">{agentId}</span>
                  <span className="rounded-pill bg-agent-tint px-2 py-0.5 text-[10px] font-medium uppercase tracking-eyebrow text-agent-fg">
                    Sub-Agent
                  </span>
                </div>
              </label>
            );
          })}
      </div>
    </section>
  );
}
