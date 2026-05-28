import {
  type ContextCompactionMode,
  type ContextWindowPolicy,
  DEFAULT_CONTEXT_POLICY,
} from "@/lib/config-api";
import { Field } from "@/components/form-components";

const COMPACTION_MODE_LABEL: Record<ContextCompactionMode, string> = {
  keep_recent_raw_suffix: "Keep recent raw suffix",
  compact_to_safe_frontier: "Compact to safe frontier",
};

export function ContextPolicySection({
  value,
  onChange,
}: {
  value: ContextWindowPolicy | null;
  onChange: (next: ContextWindowPolicy | null) => void;
}) {
  const enabled = value !== null;
  const policy = value ?? DEFAULT_CONTEXT_POLICY;
  const autocompactEnabled =
    policy.autocompact_threshold !== null && policy.autocompact_threshold !== undefined;

  function update<K extends keyof ContextWindowPolicy>(key: K, next: ContextWindowPolicy[K]) {
    onChange({ ...policy, [key]: next });
  }

  // Keep the previous value while number inputs are blank or half-typed.
  function parseTokenInput(value: string, min: number): number | undefined {
    const parsed = Number(value);
    if (!Number.isFinite(parsed) || parsed < min) return undefined;
    return parsed;
  }

  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">Context window policy</h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            Controls how the runtime trims or compacts conversation history before each inference.
            Disable to let the runtime use built-in defaults from the resolved model binding.
          </p>
        </div>
        <label className="inline-flex items-center gap-2 text-sm text-fg">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(event) =>
              onChange(event.target.checked ? { ...DEFAULT_CONTEXT_POLICY } : null)
            }
          />
          <span>Apply custom policy</span>
        </label>
      </div>

      {enabled ? (
        <div className="mt-4 grid gap-4 md:grid-cols-2">
          <Field label="Max context tokens">
            <input
              type="number"
              min={1}
              value={policy.max_context_tokens}
              onChange={(event) => {
                const next = parseTokenInput(event.target.value, 1);
                if (next !== undefined) update("max_context_tokens", next);
              }}
              className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            />
          </Field>
          <Field label="Max output tokens">
            <input
              type="number"
              min={1}
              value={policy.max_output_tokens}
              onChange={(event) => {
                const next = parseTokenInput(event.target.value, 1);
                if (next !== undefined) update("max_output_tokens", next);
              }}
              className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            />
          </Field>
          <Field label="Min recent messages">
            <input
              type="number"
              min={0}
              value={policy.min_recent_messages}
              onChange={(event) => update("min_recent_messages", Number(event.target.value) || 0)}
              className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            />
          </Field>
          <Field label="Compaction mode">
            <select
              value={policy.compaction_mode ?? "keep_recent_raw_suffix"}
              onChange={(event) =>
                update("compaction_mode", event.target.value as ContextCompactionMode)
              }
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            >
              {(Object.keys(COMPACTION_MODE_LABEL) as ContextCompactionMode[]).map((mode) => (
                <option key={mode} value={mode}>
                  {COMPACTION_MODE_LABEL[mode]}
                </option>
              ))}
            </select>
          </Field>
          <Field label="Compaction raw suffix messages">
            <input
              type="number"
              min={0}
              value={policy.compaction_raw_suffix_messages ?? 2}
              onChange={(event) =>
                update("compaction_raw_suffix_messages", Number(event.target.value) || 0)
              }
              className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            />
          </Field>
          <Field label="Autocompact threshold">
            <div className="flex items-center gap-2">
              <label className="inline-flex items-center gap-1 text-xs text-fg-soft">
                <input
                  type="checkbox"
                  checked={autocompactEnabled}
                  onChange={(event) =>
                    update(
                      "autocompact_threshold",
                      event.target.checked ? policy.max_context_tokens : null,
                    )
                  }
                />
                <span>Enable</span>
              </label>
              <input
                type="number"
                min={1}
                disabled={!autocompactEnabled}
                value={autocompactEnabled ? Number(policy.autocompact_threshold ?? 0) : ""}
                onChange={(event) => {
                  const next = parseTokenInput(event.target.value, 1);
                  if (next !== undefined) update("autocompact_threshold", next);
                }}
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg disabled:bg-muted disabled:text-fg-soft"
              />
            </div>
          </Field>
          <Field label="Prompt caching">
            <label className="inline-flex items-center gap-2 text-sm text-fg">
              <input
                type="checkbox"
                checked={Boolean(policy.enable_prompt_cache)}
                onChange={(event) => update("enable_prompt_cache", event.target.checked)}
              />
              <span>Enable prompt caching</span>
            </label>
          </Field>
        </div>
      ) : (
        <p className="mt-4 text-sm text-fg-soft">
          No custom policy applied. The runtime uses the model binding's defaults.
        </p>
      )}
    </section>
  );
}
