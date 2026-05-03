import { useMemo, useState } from "react";
import type { ToolInfo } from "@/lib/config-api";
import {
  applyToolSelectionMode,
  groupSelectionState,
  groupToolsBySource,
  isToolAllowed,
  nextAllowedTools,
  setGroupSelection,
  toolSelectionMode,
  type ToolSelectionMode,
} from "@/lib/agent-tool-selection";

interface ToolSelectorProps {
  /// Headline label rendered above the selector.
  title: string;
  /// Free-form caption explaining the semantics.
  description: string;
  /// `undefined` = mode "all"; explicit list = mode "custom".
  value: string[] | undefined;
  onChange: (next: string[] | undefined) => void;
  tools: ToolInfo[];
  /// Default mode. "include" matches `allowed_tools` semantics where
  /// undefined means "every tool". "exclude" matches `excluded_tools`
  /// semantics where undefined means "exclude none" — but the storage
  /// shape is identical (string[] | undefined), so we can reuse the
  /// component with a different label set.
  variant?: "include" | "exclude";
}

type SourceKindFilter = "all" | "builtin" | "plugin" | "mcp";

const SOURCE_KIND_LABEL: Record<Exclude<SourceKindFilter, "all">, string> = {
  builtin: "Built-in",
  plugin: "Plugin",
  mcp: "MCP",
};

export function ToolSelector({
  title,
  description,
  value,
  onChange,
  tools,
  variant = "include",
}: ToolSelectorProps) {
  const [search, setSearch] = useState("");
  const [sourceFilter, setSourceFilter] = useState<SourceKindFilter>("all");
  const allToolIds = useMemo(() => tools.map((tool) => tool.id), [tools]);
  const groups = useMemo(() => groupToolsBySource(tools), [tools]);

  const sourceTabs = useMemo(() => {
    const counts: Record<Exclude<SourceKindFilter, "all">, number> = {
      builtin: 0,
      plugin: 0,
      mcp: 0,
    };
    for (const group of groups) counts[group.source.kind] += group.tools.length;
    return [
      { key: "all" as SourceKindFilter, label: "All", count: tools.length },
      ...(["builtin", "plugin", "mcp"] as const)
        .filter((kind) => counts[kind] > 0)
        .map((kind) => ({
          key: kind as SourceKindFilter,
          label: SOURCE_KIND_LABEL[kind],
          count: counts[kind],
        })),
    ];
  }, [groups, tools.length]);

  const filteredGroups = useMemo(() => {
    const trimmed = search.trim().toLowerCase();
    return groups
      .filter((g) => sourceFilter === "all" || g.source.kind === sourceFilter)
      .map((group) => ({
        ...group,
        tools:
          trimmed.length === 0
            ? group.tools
            : group.tools.filter((tool) => {
                const haystack = `${tool.id} ${tool.name ?? ""} ${tool.description ?? ""}`
                  .toLowerCase();
                return haystack.includes(trimmed);
              }),
      }))
      .filter((group) => group.tools.length > 0);
  }, [groups, search, sourceFilter]);

  const mode = toolSelectionMode(value);
  const labels = LABELS_BY_VARIANT[variant];

  function setMode(next: ToolSelectionMode) {
    onChange(applyToolSelectionMode(value, next, allToolIds));
  }

  function toggleTool(toolId: string, checked: boolean) {
    onChange(nextAllowedTools(value, allToolIds, toolId, checked));
  }

  function toggleGroup(groupToolIds: string[], selected: boolean) {
    onChange(setGroupSelection(value, allToolIds, groupToolIds, selected));
  }

  return (
    <section className="rounded-md border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">{title}</h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">{description}</p>
        </div>
        <fieldset
          aria-label={`${title} mode`}
          className="flex shrink-0 gap-2"
        >
          <ModeRadio
            checked={mode === "all"}
            onChange={() => setMode("all")}
            label={labels.allLabel}
            hint={labels.allHint}
          />
          <ModeRadio
            checked={mode === "custom"}
            onChange={() => setMode("custom")}
            label={labels.customLabel}
            hint={labels.customHint}
          />
        </fieldset>
      </div>

      {mode === "custom" ? (
        <>
          <div
            role="tablist"
            aria-label={`${title} source filter`}
            className="mt-4 flex flex-wrap gap-1 border-b border-line"
          >
            {sourceTabs.map((tab) => {
              const active = tab.key === sourceFilter;
              return (
                <button
                  key={tab.key}
                  type="button"
                  role="tab"
                  aria-selected={active}
                  onClick={() => setSourceFilter(tab.key)}
                  className={[
                    "flex items-center gap-2 border-b-2 px-3 py-2 text-xs font-medium transition",
                    active
                      ? "border-fg-strong text-fg-strong"
                      : "border-transparent text-fg-soft hover:text-fg",
                  ].join(" ")}
                >
                  <span>{tab.label}</span>
                  <span
                    aria-hidden
                    className={[
                      "rounded-pill px-1.5 font-mono text-[10px]",
                      active ? "bg-muted text-fg-strong" : "bg-soft text-fg-soft",
                    ].join(" ")}
                  >
                    {tab.count}
                  </span>
                </button>
              );
            })}
          </div>
          <div className="mt-4">
            <label className="block">
              <span className="sr-only">Search tools</span>
              <input
                type="search"
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Search tools by id, name, or description…"
                className="w-full max-w-md rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
              />
            </label>
          </div>

          {filteredGroups.length === 0 ? (
            <div className="mt-4 rounded-md border border-dashed border-line px-4 py-3 text-sm text-fg-soft">
              No tools match the current search.
            </div>
          ) : (
            <div className="mt-4 space-y-5">
              {filteredGroups.map((group) => {
                const groupIds = group.tools.map((tool) => tool.id);
                const state = groupSelectionState(value, groupIds);
                return (
                  <div
                    key={group.source.key}
                    className="rounded-md border border-line bg-soft p-4"
                  >
                    <div className="flex flex-wrap items-center justify-between gap-3">
                      <div className="flex items-center gap-3">
                        <span className="text-sm font-semibold text-fg-strong">
                          {group.source.label}
                        </span>
                        <span className="text-xs text-fg-soft">
                          {summariseSelection(state, groupIds.length, value, groupIds)}
                        </span>
                      </div>
                      <div className="flex gap-2 text-xs font-medium">
                        <button
                          type="button"
                          onClick={() => toggleGroup(groupIds, true)}
                          className="rounded-md border border-line-strong bg-surface px-2 py-1 text-fg-soft hover:bg-muted"
                        >
                          Select all
                        </button>
                        <button
                          type="button"
                          onClick={() => toggleGroup(groupIds, false)}
                          className="rounded-md border border-line-strong bg-surface px-2 py-1 text-fg-soft hover:bg-muted"
                        >
                          Clear
                        </button>
                      </div>
                    </div>
                    <div className="mt-3 grid gap-2 md:grid-cols-2 xl:grid-cols-3">
                      {group.tools.map((tool) => {
                        const checked = isToolAllowed(value, tool.id);
                        return (
                          <label
                            key={tool.id}
                            className="flex gap-3 rounded-xl border border-line bg-surface px-3 py-2 text-sm text-fg"
                          >
                            <input
                              type="checkbox"
                              checked={checked}
                              onChange={(event) =>
                                toggleTool(tool.id, event.target.checked)
                              }
                            />
                            <div className="min-w-0">
                              <div className="font-mono text-xs text-fg-strong">
                                {tool.id}
                              </div>
                              <div className="mt-0.5 text-xs text-fg-soft">
                                {tool.description || tool.name}
                              </div>
                            </div>
                          </label>
                        );
                      })}
                    </div>
                  </div>
                );
              })}
            </div>
          )}
        </>
      ) : (
        <p className="mt-4 text-sm text-fg-soft">{labels.allBody}</p>
      )}
    </section>
  );
}

function ModeRadio({
  checked,
  onChange,
  label,
  hint,
}: {
  checked: boolean;
  onChange: () => void;
  label: string;
  hint: string;
}) {
  return (
    <label
      className={[
        "min-w-[10rem] cursor-pointer rounded-xl border px-3 py-2 text-sm transition",
        checked
          ? "border-accent bg-accent text-accent-text shadow-sm"
          : "border-line bg-surface text-fg hover:border-line-strong hover:bg-soft",
      ].join(" ")}
    >
      <input
        type="radio"
        className="sr-only"
        checked={checked}
        onChange={onChange}
      />
      <div className="font-semibold">{label}</div>
      <div
        className={[
          "mt-0.5 text-xs leading-5",
          checked ? "text-fg-faint" : "text-fg-soft",
        ].join(" ")}
      >
        {hint}
      </div>
    </label>
  );
}

function summariseSelection(
  state: "all" | "some" | "none",
  total: number,
  value: string[] | undefined,
  groupToolIds: string[],
): string {
  if (total === 0) return "0 tools";
  if (state === "all") return `${total} of ${total} selected`;
  if (state === "none") return `0 of ${total} selected`;
  let selected = 0;
  for (const id of groupToolIds) {
    if (isToolAllowed(value, id)) selected += 1;
  }
  return `${selected} of ${total} selected`;
}

const LABELS_BY_VARIANT: Record<
  "include" | "exclude",
  {
    allLabel: string;
    allHint: string;
    allBody: string;
    customLabel: string;
    customHint: string;
  }
> = {
  include: {
    allLabel: "All tools",
    allHint: "Default — every published tool is callable.",
    allBody:
      "Every tool published to the runtime stays available to this agent. Choose Custom to restrict to a specific subset.",
    customLabel: "Custom selection",
    customHint: "Only the picked tools are exposed.",
  },
  exclude: {
    allLabel: "Block none",
    allHint: "Default — nothing is removed from the allowed set.",
    allBody:
      "No tools are explicitly excluded. Switch to Custom to remove individual tools even when they appear in the allow-list.",
    customLabel: "Custom exclusion",
    customHint: "Selected tools are excluded.",
  },
};
