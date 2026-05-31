import { useMemo } from "react";
import { Link } from "react-router";
import type { Capabilities, RecordMeta } from "@/lib/config-api";
import { deriveSourceState } from "@/lib/config-api";
import { adminRoutes } from "@/lib/routes";

type VisibleToolGroup = "builtin" | "plugin" | "mcp" | "other";

export function VisibleToolDescriptors({
  tools,
  toolMetaById,
  metadataLoading,
  metadataError,
}: {
  tools: Capabilities["tools"];
  toolMetaById: Map<string, RecordMeta>;
  metadataLoading: boolean;
  metadataError: string | null;
}) {
  const groups = useMemo(() => {
    const next = new Map<VisibleToolGroup, Capabilities["tools"]>([
      ["builtin", []],
      ["plugin", []],
      ["mcp", []],
      ["other", []],
    ]);
    for (const tool of tools) {
      next.get(visibleToolGroup(tool))?.push(tool);
    }
    return [...next.entries()].filter(([, items]) => items.length > 0);
  }, [tools]);
  const editableCount = tools.filter((tool) => toolMetaById.has(tool.id)).length;

  return (
    <div className="mt-4 rounded-sm border border-line bg-soft px-3 py-3">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <div className="text-xs font-semibold uppercase tracking-eyebrow text-fg-soft">
            Final tool descriptors
          </div>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-fg-soft">
            This is the descriptor set the runtime will offer before permission hooks. Descriptions
            shown here are the effective descriptions the model sees; stored tools can be tuned
            without changing code.
          </p>
        </div>
        <Link
          to={adminRoutes.tools}
          className="rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs font-medium text-fg-soft hover:bg-muted hover:text-fg"
        >
          Open tool catalog
        </Link>
      </div>

      {metadataError ? (
        <div className="mt-3 rounded-sm border border-tone-warn/35 bg-tone-warn/10 px-3 py-2 text-xs text-tone-warn">
          Tool override metadata unavailable: {metadataError}
        </div>
      ) : null}
      {metadataLoading ? (
        <div className="mt-3 text-xs text-fg-soft">Loading tool override metadata...</div>
      ) : null}

      <div className="mt-3 flex flex-wrap gap-2 text-[11px] text-fg-soft">
        <span className="rounded-pill bg-muted px-2 py-0.5">Override-ready: {editableCount}</span>
        <span className="rounded-pill bg-muted px-2 py-0.5">
          MCP: {tools.filter((tool) => visibleToolGroup(tool) === "mcp").length}
        </span>
      </div>

      <div className="mt-4 space-y-4">
        {groups.map(([group, items]) => (
          <div key={group}>
            <h4 className="text-xs font-semibold uppercase tracking-eyebrow text-fg-soft">
              {visibleToolGroupLabel(group)} ({items.length})
            </h4>
            <ul className="mt-2 grid gap-2 xl:grid-cols-2">
              {items.map((tool) => (
                <ToolDescriptorCard
                  key={tool.id}
                  tool={tool}
                  meta={toolMetaById.get(tool.id) ?? null}
                />
              ))}
            </ul>
          </div>
        ))}
      </div>
    </div>
  );
}

function ToolDescriptorCard({
  tool,
  meta,
}: {
  tool: Capabilities["tools"][number];
  meta: RecordMeta | null;
}) {
  const sourceLabel = visibleToolSourceLabel(tool);
  const state = meta ? deriveSourceState(meta) : null;
  return (
    <li className="rounded-sm border border-line bg-surface px-3 py-3 text-sm text-fg">
      <div className="flex min-w-0 flex-wrap items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="break-all font-mono text-xs font-semibold text-fg-strong">{tool.id}</div>
          {tool.name && tool.name !== tool.id ? (
            <div className="mt-1 truncate text-sm font-medium text-fg">{tool.name}</div>
          ) : null}
        </div>
        <div className="flex shrink-0 flex-wrap justify-end gap-1">
          <span className="rounded-pill bg-muted px-2 py-0.5 text-[10px] font-medium text-fg-soft">
            {sourceLabel}
          </span>
          {state === "customized" ? (
            <span className="rounded-pill bg-state-progress px-2 py-0.5 text-[10px] font-medium text-fg">
              custom description
            </span>
          ) : null}
        </div>
      </div>
      <p className="mt-2 line-clamp-3 text-xs leading-5 text-fg-soft" title={tool.description}>
        {tool.description || "No description supplied."}
      </p>
      <div className="mt-3 flex flex-wrap items-center gap-2 text-[11px] text-fg-soft">
        {meta ? (
          <Link
            to={`${adminRoutes.tool(tool.id)}?edit=description`}
            className="rounded-sm border border-line bg-soft px-2 py-0.5 font-medium hover:bg-muted hover:text-fg"
          >
            Override description
          </Link>
        ) : (
          <span className="rounded-sm border border-line bg-soft px-2 py-0.5">
            Source-owned descriptor
          </span>
        )}
        {visibleToolGroup(tool) === "mcp" ? (
          <span className="text-fg-faint">MCP descriptions come from the verified server.</span>
        ) : null}
      </div>
    </li>
  );
}

function visibleToolGroup(tool: Capabilities["tools"][number]): VisibleToolGroup {
  if (tool.source?.kind === "mcp" || tool.id.startsWith("mcp__")) return "mcp";
  if (tool.source?.kind === "plugin") return "plugin";
  if (tool.source?.kind === "builtin") return "builtin";
  return "other";
}

function visibleToolGroupLabel(group: VisibleToolGroup): string {
  switch (group) {
    case "builtin":
      return "Built-in tools";
    case "plugin":
      return "Plugin tools";
    case "mcp":
      return "MCP tools";
    case "other":
      return "Other tools";
  }
}

function visibleToolSourceLabel(tool: Capabilities["tools"][number]): string {
  if (tool.source?.kind === "mcp") return tool.source.id ? `MCP ${tool.source.id}` : "MCP";
  if (tool.source?.kind === "plugin") {
    return tool.source.id ? `plugin ${tool.source.id}` : "plugin";
  }
  if (tool.id.startsWith("mcp__")) {
    const serverId = tool.id.slice("mcp__".length).split("__")[0];
    return serverId ? `MCP ${serverId}` : "MCP";
  }
  return "built-in";
}
