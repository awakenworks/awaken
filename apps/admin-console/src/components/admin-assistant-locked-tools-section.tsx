import type { Capabilities } from "@/lib/config-api";

export function AdminAssistantLockedToolsSection({ capabilities }: { capabilities: Capabilities }) {
  const tools = capabilities.admin_assistant?.bound_tools ?? [];
  if (tools.length === 0) return null;

  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">Admin Assistant tools</h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            These management tools are directly bound to the server-managed Admin Assistant. They
            are visible here for comparison, but they are not registry tools and cannot be assigned
            to a normal Agent.
          </p>
        </div>
        <span className="rounded-pill bg-muted px-2 py-0.5 text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
          locked
        </span>
      </div>
      <div className="mt-4 grid gap-2 md:grid-cols-2 xl:grid-cols-3">
        {tools.map((tool) => (
          <div key={tool.id} className="rounded-sm border border-line bg-soft px-3 py-2 text-sm">
            <div className="font-mono text-xs font-medium text-fg-strong">{tool.id}</div>
            <div className="mt-1 text-xs font-medium text-fg">{tool.label}</div>
            <p className="mt-1 line-clamp-2 text-xs leading-5 text-fg-soft">
              {tool.description}
            </p>
            <div className="mt-2 flex flex-wrap gap-1 text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft">
              <span className="rounded-pill bg-muted px-2 py-0.5">admin only</span>
              <span className="rounded-pill bg-muted px-2 py-0.5">not selectable</span>
              {tool.requires_confirmation ? (
                <span className="rounded-pill bg-tone-warn/15 px-2 py-0.5 text-tone-warn">
                  confirmation
                </span>
              ) : null}
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
