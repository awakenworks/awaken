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

export function ToolSelector({
  title,
  description,
  value,
  onChange,
  tools,
  variant = "include",
}: ToolSelectorProps) {
  const [search, setSearch] = useState("");
  const allToolIds = useMemo(() => tools.map((tool) => tool.id), [tools]);
  const groups = useMemo(() => groupToolsBySource(tools), [tools]);

  const filteredGroups = useMemo(() => {
    const trimmed = search.trim().toLowerCase();
    if (trimmed.length === 0) return groups;
    return groups
      .map((group) => ({
        ...group,
        tools: group.tools.filter((tool) => {
          const haystack = `${tool.id} ${tool.name ?? ""} ${tool.description ?? ""}`
            .toLowerCase();
          return haystack.includes(trimmed);
        }),
      }))
      .filter((group) => group.tools.length > 0);
  }, [groups, search]);

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
    <section className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-slate-950">{title}</h3>
          <p className="mt-2 max-w-xl text-sm text-slate-500">{description}</p>
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
          <div className="mt-4">
            <label className="block">
              <span className="sr-only">Search tools</span>
              <input
                type="search"
                value={search}
                onChange={(event) => setSearch(event.target.value)}
                placeholder="Search tools by id, name, or description…"
                className="w-full max-w-md rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
              />
            </label>
          </div>

          {filteredGroups.length === 0 ? (
            <div className="mt-4 rounded-2xl border border-dashed border-slate-200 px-4 py-3 text-sm text-slate-500">
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
                    className="rounded-2xl border border-slate-200 bg-slate-50 p-4"
                  >
                    <div className="flex flex-wrap items-center justify-between gap-3">
                      <div className="flex items-center gap-3">
                        <span className="text-sm font-semibold text-slate-900">
                          {group.source.label}
                        </span>
                        <span className="text-xs text-slate-500">
                          {summariseSelection(state, groupIds.length, value, groupIds)}
                        </span>
                      </div>
                      <div className="flex gap-2 text-xs font-medium">
                        <button
                          type="button"
                          onClick={() => toggleGroup(groupIds, true)}
                          className="rounded-md border border-slate-300 bg-white px-2 py-1 text-slate-600 hover:bg-slate-100"
                        >
                          Select all
                        </button>
                        <button
                          type="button"
                          onClick={() => toggleGroup(groupIds, false)}
                          className="rounded-md border border-slate-300 bg-white px-2 py-1 text-slate-600 hover:bg-slate-100"
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
                            className="flex gap-3 rounded-xl border border-slate-200 bg-white px-3 py-2 text-sm text-slate-700"
                          >
                            <input
                              type="checkbox"
                              checked={checked}
                              onChange={(event) =>
                                toggleTool(tool.id, event.target.checked)
                              }
                            />
                            <div className="min-w-0">
                              <div className="font-mono text-xs text-slate-900">
                                {tool.id}
                              </div>
                              <div className="mt-0.5 text-xs text-slate-500">
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
        <p className="mt-4 text-sm text-slate-500">{labels.allBody}</p>
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
          ? "border-slate-900 bg-slate-900 text-white shadow-sm"
          : "border-slate-200 bg-white text-slate-700 hover:border-slate-300 hover:bg-slate-50",
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
          checked ? "text-slate-200" : "text-slate-500",
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
