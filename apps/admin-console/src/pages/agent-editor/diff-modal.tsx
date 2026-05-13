import { type AgentSpec } from "@/lib/config-api";

interface FieldChange {
  path: string;
  before: unknown;
  after: unknown;
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (a === null || b === null) return a === b;
  if (typeof a !== typeof b) return false;
  if (typeof a !== "object") return false;
  return JSON.stringify(a) === JSON.stringify(b);
}

function computeDiff(
  prev: Record<string, unknown>,
  curr: Record<string, unknown>,
  base = "",
): FieldChange[] {
  const out: FieldChange[] = [];
  const keys = new Set([...Object.keys(prev ?? {}), ...Object.keys(curr ?? {})]);
  for (const key of keys) {
    const path = base ? `${base}.${key}` : key;
    const a = prev?.[key];
    const b = curr?.[key];
    if (deepEqual(a, b)) continue;
    if (
      a !== null &&
      b !== null &&
      typeof a === "object" &&
      typeof b === "object" &&
      !Array.isArray(a) &&
      !Array.isArray(b)
    ) {
      out.push(...computeDiff(a as Record<string, unknown>, b as Record<string, unknown>, path));
    } else {
      out.push({ path, before: a, after: b });
    }
  }
  return out;
}

function formatDiffValue(value: unknown): string {
  if (value === undefined) return "(unset)";
  if (value === null) return "null";
  if (typeof value === "string") return value || "(empty string)";
  return JSON.stringify(value, null, 2);
}

export function DiffModal({
  current,
  previous,
  onClose,
}: {
  current: AgentSpec;
  previous: AgentSpec;
  onClose: () => void;
}) {
  const changes = computeDiff(
    previous as unknown as Record<string, unknown>,
    current as unknown as Record<string, unknown>,
  );
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Diff vs published"
      className="fixed inset-0 z-50 flex items-center justify-center bg-overlay px-4 backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        className="w-full max-w-3xl max-h-[80vh] overflow-hidden rounded-lg bg-surface shadow-overlay flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between border-b border-line px-5 py-3">
          <div>
            <h3 className="text-base font-semibold text-fg-strong">Diff vs published</h3>
            <p className="mt-0.5 text-xs text-fg-soft">
              {changes.length} field{changes.length === 1 ? "" : "s"} would change on save.
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            className="rounded-md border border-line bg-soft px-2 py-1 text-xs text-fg-soft hover:text-fg"
          >
            Close
          </button>
        </div>
        <div className="overflow-y-auto p-5">
          {changes.length === 0 ? (
            <p className="text-sm text-fg-soft">
              No semantic changes. (The dirty flag may be set because of a transient form edit;
              saving is safe.)
            </p>
          ) : (
            <ul className="space-y-3">
              {changes.map((change) => (
                <li key={change.path} className="rounded-md border border-line bg-soft p-3">
                  <div className="font-mono text-xs font-medium text-fg-strong">{change.path}</div>
                  <div className="mt-2 grid gap-2 md:grid-cols-2">
                    <div>
                      <div className="text-[10px] font-medium uppercase tracking-eyebrow text-tone-error">
                        Was
                      </div>
                      <pre className="mt-1 overflow-auto rounded border border-tone-error/20 bg-tone-error/5 px-2 py-1 font-mono text-xs text-fg">
                        {formatDiffValue(change.before)}
                      </pre>
                    </div>
                    <div>
                      <div className="text-[10px] font-medium uppercase tracking-eyebrow text-tone-success">
                        Will be
                      </div>
                      <pre className="mt-1 overflow-auto rounded border border-tone-success/20 bg-tone-success/5 px-2 py-1 font-mono text-xs text-fg">
                        {formatDiffValue(change.after)}
                      </pre>
                    </div>
                  </div>
                </li>
              ))}
            </ul>
          )}
        </div>
      </div>
    </div>
  );
}
