import { useMemo, useState } from "react";
import { ToolPatternHelp } from "@/components/tool-pattern-help";
import { patternMatchesIn } from "@/lib/tool-catalog";
import {
  TOOL_ID_PATTERN_PLACEHOLDER,
  normalizeToolPatternInput,
  validateToolPatternInput,
} from "@/lib/tool-pattern-guidance";

export interface ToolSelectorDetail {
  id: string;
  name?: string;
  description?: string;
  source?: { kind: "builtin" | "plugin" | "mcp"; id?: string };
}

export interface ToolSelectorProps {
  /// Drives the heading and quick-action button copy. The same component
  /// renders both the allow-list and deny-list panels on the agent
  /// editor — the only externally visible difference is this label.
  label: "Allowed" | "Excluded";
  /// Tool ids currently visible from the runtime registry. Used both to
  /// render the literal checkbox list and to compute live match previews
  /// for each pattern entry.
  registered: readonly string[];
  /// Current literal entries (user's explicit per-tool selection).
  literals: readonly string[];
  /// Current pattern entries (user-authored anchored globs).
  patterns: readonly string[];
  /// Optional descriptors for registered ids. When present, the selector
  /// shows the same tool descriptions that the model receives.
  toolDetails?: readonly ToolSelectorDetail[];
  /// Called with the new {literals, patterns} pair whenever the user
  /// changes anything. The caller owns the data — this component holds
  /// no state beyond the in-progress pattern draft.
  onChange: (next: { literals: string[]; patterns: string[] }) => void;
}

export function ToolSelector({
  label,
  registered,
  literals,
  patterns,
  toolDetails = [],
  onChange,
}: ToolSelectorProps) {
  const [draftPattern, setDraftPattern] = useState("");
  const [draftError, setDraftError] = useState<string | null>(null);
  const literalSet = useMemo(() => new Set(literals), [literals]);
  const detailById = useMemo(
    () => new Map(toolDetails.map((tool) => [tool.id, tool])),
    [toolDetails],
  );

  function toggleLiteral(id: string) {
    const next = literalSet.has(id) ? literals.filter((x) => x !== id) : [...literals, id];
    onChange({ literals: next, patterns: [...patterns] });
  }

  function addPattern() {
    const trimmed = normalizeToolPatternInput(draftPattern);
    const error = validateToolPatternInput(trimmed, { kind: "tool-id", allowEmpty: true });
    if (error) {
      setDraftError(error);
      return;
    }
    if (!trimmed || patterns.includes(trimmed)) {
      setDraftPattern("");
      setDraftError(null);
      return;
    }
    onChange({ literals: [...literals], patterns: [...patterns, trimmed] });
    setDraftPattern("");
    setDraftError(null);
  }

  function removePattern(p: string) {
    onChange({
      literals: [...literals],
      patterns: patterns.filter((x) => x !== p),
    });
  }

  function addUniversalPattern() {
    if (patterns.includes("*")) return;
    onChange({ literals: [...literals], patterns: ["*", ...patterns] });
  }

  function seedLiteralsFromRegistered() {
    onChange({ literals: [...registered], patterns: [...patterns] });
  }

  const universalLabel = label === "Allowed" ? "Allow all tools" : "Exclude all tools";

  return (
    <section
      data-testid={`tool-selector-${label.toLowerCase()}`}
      className="rounded-sm border border-line bg-surface p-5 shadow-sm"
    >
      <h3 className="text-lg font-semibold text-fg-strong">{label} tools</h3>

      <fieldset className="mt-4 rounded-sm border border-line bg-soft p-4">
        <legend className="px-2 text-sm font-semibold text-fg-strong">Tools (literal)</legend>
        {registered.length === 0 ? (
          <p className="text-sm text-fg-soft">No tools registered.</p>
        ) : (
          <div className="grid gap-2 md:grid-cols-2 xl:grid-cols-3">
            {registered.map((id) => {
              const detail = detailById.get(id);
              return (
                <label
                  key={id}
                  className="flex min-h-[5.25rem] items-start gap-2 rounded-sm border border-line bg-surface px-3 py-2 text-sm text-fg"
                >
                  <input
                    className="mt-1"
                    type="checkbox"
                    checked={literalSet.has(id)}
                    onChange={() => toggleLiteral(id)}
                  />
                  <span className="min-w-0 flex-1">
                    <span className="flex min-w-0 flex-wrap items-center gap-2">
                      <span className="break-all font-mono text-xs text-fg-strong">{id}</span>
                      {detail?.source ? (
                        <span className="rounded-pill bg-muted px-2 py-0.5 text-[10px] font-medium text-fg-soft">
                          {toolSourceLabel(detail.source)}
                        </span>
                      ) : null}
                    </span>
                    {detail?.name && detail.name !== id ? (
                      <span className="mt-1 block truncate text-xs font-medium text-fg">
                        {detail.name}
                      </span>
                    ) : null}
                    {detail?.description ? (
                      <span className="mt-1 line-clamp-2 block text-xs leading-5 text-fg-soft">
                        {detail.description}
                      </span>
                    ) : null}
                  </span>
                </label>
              );
            })}
          </div>
        )}
        <div className="mt-3">
          <button
            type="button"
            onClick={seedLiteralsFromRegistered}
            className="rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs font-medium text-fg-soft hover:bg-muted"
          >
            Seed literals from registry
          </button>
        </div>
      </fieldset>

      <fieldset className="mt-4 rounded-sm border border-line bg-soft p-4">
        <legend className="px-2 text-sm font-semibold text-fg-strong">Patterns</legend>
        <ToolPatternHelp kind="tool-id" />
        {patterns.length === 0 ? (
          <p className="mt-3 text-sm text-fg-soft">No patterns defined.</p>
        ) : (
          <ul className="mt-3 space-y-2">
            {patterns.map((p) => {
              const hits = patternMatchesIn(p, registered);
              const summary =
                hits.length === 0
                  ? "matches none"
                  : `matches ${hits.length}: ${hits.slice(0, 3).join(", ")}${
                      hits.length > 3 ? "…" : ""
                    }`;
              return (
                <li
                  key={p}
                  className="flex items-center justify-between gap-3 rounded-sm border border-line bg-surface px-3 py-2 text-sm text-fg"
                >
                  <div className="flex min-w-0 flex-1 flex-wrap items-center gap-3">
                    <code className="font-mono text-xs text-fg-strong">{p}</code>
                    <small
                      className={
                        hits.length === 0 ? "text-xs text-fg-soft italic" : "text-xs text-fg-soft"
                      }
                    >
                      {summary}
                    </small>
                  </div>
                  <button
                    type="button"
                    onClick={() => removePattern(p)}
                    aria-label={`Remove pattern ${p}`}
                    className="rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs font-medium text-fg-soft hover:bg-muted"
                  >
                    Remove
                  </button>
                </li>
              );
            })}
          </ul>
        )}
        <div className="mt-3 flex flex-wrap items-center gap-2">
          <label className="flex-1 min-w-[16rem]">
            <span className="sr-only">New pattern</span>
            <input
              type="text"
              value={draftPattern}
              onChange={(e) => {
                setDraftPattern(e.target.value);
                if (draftError) {
                  setDraftError(null);
                }
              }}
              placeholder={`e.g. ${TOOL_ID_PATTERN_PLACEHOLDER}`}
              aria-invalid={draftError ? true : undefined}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  addPattern();
                }
              }}
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            />
          </label>
          <button
            type="button"
            onClick={addPattern}
            className="rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs font-medium text-fg-soft hover:bg-muted"
          >
            Add pattern
          </button>
          <button
            type="button"
            onClick={addUniversalPattern}
            className="rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs font-medium text-fg-soft hover:bg-muted"
          >
            {universalLabel}
          </button>
        </div>
        {draftError ? (
          <div role="alert" className="mt-2 text-xs text-tone-error">
            {draftError}
          </div>
        ) : null}
      </fieldset>
    </section>
  );
}

function toolSourceLabel(source: ToolSelectorDetail["source"]): string {
  if (!source) return "tool";
  if (source.kind === "mcp") return source.id ? `MCP ${source.id}` : "MCP";
  if (source.kind === "plugin") return source.id ? `plugin ${source.id}` : "plugin";
  return "built-in";
}
