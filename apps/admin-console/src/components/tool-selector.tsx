import { useMemo, useState } from "react";
import { patternMatchesIn } from "@/lib/tool-catalog";

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
  onChange,
}: ToolSelectorProps) {
  const [draftPattern, setDraftPattern] = useState("");
  const literalSet = useMemo(() => new Set(literals), [literals]);

  function toggleLiteral(id: string) {
    const next = literalSet.has(id)
      ? literals.filter((x) => x !== id)
      : [...literals, id];
    onChange({ literals: next, patterns: [...patterns] });
  }

  function addPattern() {
    const trimmed = draftPattern.trim();
    if (!trimmed || patterns.includes(trimmed)) {
      setDraftPattern("");
      return;
    }
    onChange({ literals: [...literals], patterns: [...patterns, trimmed] });
    setDraftPattern("");
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
        <legend className="px-2 text-sm font-semibold text-fg-strong">
          Tools (literal)
        </legend>
        {registered.length === 0 ? (
          <p className="text-sm text-fg-soft">No tools registered.</p>
        ) : (
          <div className="grid gap-2 md:grid-cols-2 xl:grid-cols-3">
            {registered.map((id) => (
              <label
                key={id}
                className="flex items-center gap-2 rounded-sm border border-line bg-surface px-3 py-2 text-sm text-fg"
              >
                <input
                  type="checkbox"
                  checked={literalSet.has(id)}
                  onChange={() => toggleLiteral(id)}
                />
                <span className="font-mono text-xs text-fg-strong">{id}</span>
              </label>
            ))}
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
        <legend className="px-2 text-sm font-semibold text-fg-strong">
          Patterns
        </legend>
        {patterns.length === 0 ? (
          <p className="text-sm text-fg-soft">No patterns defined.</p>
        ) : (
          <ul className="space-y-2">
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
                        hits.length === 0
                          ? "text-xs text-fg-soft italic"
                          : "text-xs text-fg-soft"
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
              onChange={(e) => setDraftPattern(e.target.value)}
              placeholder="e.g. mcp:*"
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
      </fieldset>
    </section>
  );
}
