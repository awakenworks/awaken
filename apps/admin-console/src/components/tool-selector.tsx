import { useEffect, useMemo, useState } from "react";
import type { ToolInfo } from "@/lib/config-api";
import {
  applyToolSelectionMode,
  catalogEntryInspections,
  groupSelectionState,
  groupToolsBySource,
  isLegacyCatalogValue,
  isToolAllowed,
  isToolSelectionPatternBacked,
  toolSelectionPattern,
  nextAllowedTools,
  removeCatalogEntry,
  setGroupSelection,
  toolSelectionMode,
  type CatalogEntryInspection,
  type CatalogVariant,
  type ToolSelectionMode,
} from "@/lib/agent-tool-selection";

interface ToolSelectorProps {
  /// Headline label rendered above the selector.
  title: string;
  /// Free-form caption explaining the semantics.
  description: string;
  /// New explicit semantics: `["*"]` = all, `[]` = none, list = subset.
  /// `null`/`undefined` accepted as legacy and surfaces a deprecation hint.
  value: string[] | null | undefined;
  onChange: (next: string[]) => void;
  tools: ToolInfo[];
  /// "include" — emits `["*"]` for All. "exclude" — emits `[]` for Block none.
  /// The auto-collapse to `["*"]` when every tool ends up selected applies
  /// only to the include variant.
  variant?: CatalogVariant;
  overridden?: boolean;
  onReset?: () => void;
  resetLabel?: string;
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
  overridden = false,
  onReset,
  resetLabel,
}: ToolSelectorProps) {
  const [search, setSearch] = useState("");
  const [sourceFilter, setSourceFilter] = useState<SourceKindFilter>("all");
  const [modeOverride, setModeOverride] = useState<ToolSelectionMode | null>(null);
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
                const haystack =
                  `${tool.id} ${tool.name ?? ""} ${tool.description ?? ""}`.toLowerCase();
                return haystack.includes(trimmed);
              }),
      }))
      .filter((group) => group.tools.length > 0);
  }, [groups, search, sourceFilter]);

  const derivedMode = toolSelectionMode(value, variant);
  const mode = modeOverride ?? derivedMode;
  const labels = LABELS_BY_VARIANT[variant];
  const catalogEntries = useMemo(
    () => catalogEntryInspections(value, allToolIds, variant),
    [allToolIds, value, variant],
  );

  useEffect(() => {
    if (!modeOverride) return;
    if (modeOverride === derivedMode) {
      setModeOverride(null);
      return;
    }
    if (
      modeOverride === "custom" &&
      variant === "exclude" &&
      (value == null || (Array.isArray(value) && value.length === 0))
    ) {
      return;
    }
    setModeOverride(null);
  }, [derivedMode, modeOverride, value, variant]);

  function setMode(next: ToolSelectionMode) {
    const nextValue = applyToolSelectionMode(value, next, allToolIds, variant);
    setModeOverride(toolSelectionMode(nextValue, variant) === next ? null : next);
    onChange(nextValue);
  }

  function toggleTool(toolId: string, checked: boolean) {
    if (isToolSelectionPatternBacked(value, toolId, variant)) return;
    onChange(nextAllowedTools(value, allToolIds, toolId, checked, variant));
  }

  function toggleGroup(groupToolIds: string[], selected: boolean) {
    if (!selected && groupToolIds.some((id) => isToolSelectionPatternBacked(value, id, variant))) {
      return;
    }
    onChange(setGroupSelection(value, allToolIds, groupToolIds, selected, variant));
  }

  function removeEntry(entry: string) {
    onChange(removeCatalogEntry(value, entry));
  }

  const legacy = isLegacyCatalogValue(value);
  const legacyHint = LEGACY_HINT_BY_VARIANT[variant];

  return (
    <section className="rounded-md border border-line bg-surface p-5 shadow-sm">
      {legacy ? (
        <div
          role="status"
          className="mb-4 rounded-md border border-line-strong bg-soft px-3 py-2 text-xs text-fg"
        >
          {legacyHint}
        </div>
      ) : null}
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <div className="flex items-center gap-2">
            <h3 className="text-lg font-semibold text-fg-strong">{title}</h3>
            {overridden && onReset ? (
              <button
                type="button"
                onClick={onReset}
                aria-label={resetLabel ?? `Reset ${title} to default`}
                title={resetLabel ?? `Reset ${title} to default`}
                className="inline-flex h-5 w-5 items-center justify-center rounded-full text-xs text-tone-warn transition hover:bg-tone-warn/15"
              >
                ↺
              </button>
            ) : null}
          </div>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">{description}</p>
        </div>
        <fieldset aria-label={`${title} mode`} className="flex shrink-0 gap-2">
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
              {tools.length === 0 ? "No tools are currently published." : "No tools match the current search."}
            </div>
          ) : (
            <div className="mt-4 space-y-5">
              {filteredGroups.map((group) => {
                const groupIds = group.tools.map((tool) => tool.id);
                const state = groupSelectionState(value, groupIds, variant);
                const hasPatternBackedSelection = groupIds.some((id) =>
                  isToolSelectionPatternBacked(value, id, variant),
                );
                return (
                  <div key={group.source.key} className="rounded-md border border-line bg-soft p-4">
                    <div className="flex flex-wrap items-center justify-between gap-3">
                      <div className="flex items-center gap-3">
                        <span className="text-sm font-semibold text-fg-strong">
                          {group.source.label}
                        </span>
                        <span className="text-xs text-fg-soft">
                          {summariseSelection(state, groupIds.length, value, groupIds, variant)}
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
                          disabled={hasPatternBackedSelection}
                          title={
                            hasPatternBackedSelection
                              ? "This group includes pattern-backed entries that cannot be cleared here."
                              : undefined
                          }
                          className="rounded-md border border-line-strong bg-surface px-2 py-1 text-fg-soft hover:bg-muted"
                        >
                          Clear
                        </button>
                      </div>
                    </div>
                    <div className="mt-3 grid gap-2 md:grid-cols-2 xl:grid-cols-3">
                      {group.tools.map((tool) => {
                        const checked = isToolAllowed(value, tool.id, variant);
                        const matchingPattern = toolSelectionPattern(value, tool.id, variant);
                        const patternBacked = matchingPattern !== null;
                        return (
                          <label
                            key={tool.id}
                            title={
                              patternBacked
                                ? `Matched by tool-id pattern \`${matchingPattern}\`.`
                                : undefined
                            }
                            className={[
                              "flex gap-3 rounded-xl border border-line bg-surface px-3 py-2 text-sm text-fg",
                              patternBacked ? "opacity-70" : "",
                            ].join(" ")}
                          >
                            <input
                              type="checkbox"
                              checked={checked}
                              disabled={patternBacked}
                              onChange={(event) => toggleTool(tool.id, event.target.checked)}
                            />
                            <div className="min-w-0">
                              <div className="flex flex-wrap items-center gap-2">
                                <div className="font-mono text-xs text-fg-strong">{tool.id}</div>
                                {patternBacked ? (
                                  <span
                                    title={`Pattern: ${matchingPattern}`}
                                    className="rounded-pill border border-line-strong bg-soft px-1.5 font-mono text-[10px] font-medium text-fg-soft"
                                  >
                                    {matchingPattern}
                                  </span>
                                ) : null}
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
      {catalogEntries.length > 0 ? (
        <CatalogEntryList entries={catalogEntries} onRemove={removeEntry} />
      ) : null}
    </section>
  );
}

function CatalogEntryList({
  entries,
  onRemove,
}: {
  entries: CatalogEntryInspection[];
  onRemove: (entry: string) => void;
}) {
  return (
    <div className="mt-4 rounded-md border border-line bg-soft p-4">
      <div className="text-sm font-semibold text-fg-strong">Pattern / unmanaged catalog entries</div>
      <div className="mt-3 space-y-2">
        {entries.map((entry) => (
          <div
            key={entry.entry}
            className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-line bg-surface px-3 py-2 text-sm"
          >
            <div className="min-w-0">
              <div className="flex flex-wrap items-center gap-2">
                <span className="font-mono text-xs text-fg-strong">{entry.entry}</span>
                {entry.usesWildcard ? (
                  <span className="rounded-pill bg-muted px-1.5 text-[10px] font-medium text-fg-soft">
                    wildcard
                  </span>
                ) : null}
                {entry.escapedLiteral ? (
                  <span className="rounded-pill bg-muted px-1.5 text-[10px] font-medium text-fg-soft">
                    escaped literal
                  </span>
                ) : null}
                {!entry.exactToolExists && !entry.matchesCurrentToolOnly ? (
                  <span className="rounded-pill bg-muted px-1.5 text-[10px] font-medium text-fg-soft">
                    unmanaged
                  </span>
                ) : null}
              </div>
              <div className="mt-1 text-xs text-fg-soft">{catalogEntryMatchSummary(entry)}</div>
            </div>
            <button
              type="button"
              onClick={() => onRemove(entry.entry)}
              className="rounded-md border border-line-strong bg-surface px-2 py-1 text-xs font-medium text-fg-soft hover:bg-muted"
            >
              Remove
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}

function catalogEntryMatchSummary(entry: CatalogEntryInspection): string {
  if (entry.matches.length === 0) return "No current tool matches";
  if (entry.escapedLiteral) return `Escaped literal for ${entry.matches[0]}`;
  const shown = entry.matches.slice(0, 4).join(", ");
  const remaining = entry.matches.length - 4;
  return remaining > 0 ? `Matches ${shown} +${remaining} more` : `Matches ${shown}`;
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
      <input type="radio" className="sr-only" checked={checked} onChange={onChange} />
      <div className="font-semibold">{label}</div>
      <div
        className={["mt-0.5 text-xs leading-5", checked ? "text-fg-faint" : "text-fg-soft"].join(
          " ",
        )}
      >
        {hint}
      </div>
    </label>
  );
}

function summariseSelection(
  state: "all" | "some" | "none",
  total: number,
  value: string[] | null | undefined,
  groupToolIds: string[],
  variant: CatalogVariant,
): string {
  if (total === 0) return "0 tools";
  if (state === "all") return `${total} of ${total} selected`;
  if (state === "none") return `0 of ${total} selected`;
  let selected = 0;
  for (const id of groupToolIds) {
    if (isToolAllowed(value, id, variant)) selected += 1;
  }
  return `${selected} of ${total} selected`;
}

const LEGACY_HINT_BY_VARIANT: Record<CatalogVariant, string> = {
  include:
    'Legacy config detected: allowed_tools is null. Runtime treats this as the explicit ["*"] form.',
  exclude:
    "Legacy config detected: excluded_tools is null. Runtime treats this as the explicit [] form.",
};

const LABELS_BY_VARIANT: Record<
  CatalogVariant,
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
