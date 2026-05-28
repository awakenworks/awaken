import {
  type CompactionConfig,
  type CompactionExecutionMode,
  DEFAULT_COMPACTION_CONFIG,
} from "@/lib/config-api";
import { Field } from "@/components/form-components";

const COMPACTION_EXECUTION_LABEL: Record<CompactionExecutionMode, string> = {
  background: "Background",
  off: "Off",
};

function readCompactionConfig(value: unknown): CompactionConfig | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  return {
    ...DEFAULT_COMPACTION_CONFIG,
    ...(value as Partial<CompactionConfig>),
  };
}

export function CompactionSection({
  value,
  onChange,
}: {
  value: unknown;
  onChange: (next: CompactionConfig | null) => void;
}) {
  const resolved = readCompactionConfig(value);
  const enabled = resolved !== null;
  const config = resolved ?? DEFAULT_COMPACTION_CONFIG;

  function update<K extends keyof CompactionConfig>(key: K, next: CompactionConfig[K]) {
    onChange({ ...config, [key]: next });
  }

  function parseOptionalPositiveInteger(value: string): number | null | undefined {
    const trimmed = value.trim();
    if (!trimmed) return null;
    const parsed = Number(trimmed);
    if (!Number.isFinite(parsed) || parsed < 1) return undefined;
    return Math.trunc(parsed);
  }

  function parseRatio(value: string): number | undefined {
    const parsed = Number(value);
    if (!Number.isFinite(parsed)) return undefined;
    return Math.max(0, Math.min(1, parsed));
  }

  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h3 className="text-lg font-semibold text-fg-strong">Compaction summarizer</h3>
          <p className="mt-2 max-w-xl text-sm text-fg-soft">
            Tunes how automatic context compaction summarizes older messages. The runtime keeps the
            latest user turn raw, writes summaries into the prompt window, and preserves durable
            thread history.
          </p>
        </div>
        <label className="inline-flex items-center gap-2 text-sm text-fg">
          <input
            type="checkbox"
            checked={enabled}
            onChange={(event) =>
              onChange(event.target.checked ? { ...DEFAULT_COMPACTION_CONFIG } : null)
            }
          />
          <span>Customize summarizer</span>
        </label>
      </div>

      {enabled ? (
        <div className="mt-4 grid gap-4 md:grid-cols-2">
          <Field label="Execution mode">
            <select
              value={config.mode ?? "background"}
              onChange={(event) => update("mode", event.target.value as CompactionExecutionMode)}
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            >
              {(Object.keys(COMPACTION_EXECUTION_LABEL) as CompactionExecutionMode[]).map(
                (mode) => (
                  <option key={mode} value={mode}>
                    {COMPACTION_EXECUTION_LABEL[mode]}
                  </option>
                ),
              )}
            </select>
          </Field>
          <div>
            <Field label="Summary upstream model override">
              <input
                type="text"
                value={config.summary_model ?? ""}
                onChange={(event) => update("summary_model", event.target.value.trim() || null)}
                placeholder="Default: agent model"
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
              />
            </Field>
            <p className="mt-1 text-xs leading-5 text-fg-soft">
              Uses the same resolved provider/executor as the agent. A registry model id is accepted
              only when it belongs to the same provider; otherwise use that provider's upstream model
              name directly.
            </p>
          </div>
          <Field label="Summary max tokens">
            <input
              type="number"
              min={1}
              value={config.summary_max_tokens ?? ""}
              onChange={(event) => {
                const next = parseOptionalPositiveInteger(event.target.value);
                if (next !== undefined) update("summary_max_tokens", next);
              }}
              placeholder="1024"
              className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            />
          </Field>
          <Field label="Minimum savings ratio">
            <input
              type="number"
              min={0}
              max={1}
              step="0.01"
              value={config.min_savings_ratio}
              onChange={(event) => {
                const next = parseRatio(event.target.value);
                if (next !== undefined) update("min_savings_ratio", next);
              }}
              className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            />
          </Field>
          <Field label="Raw message retention">
            <select
              value={config.raw_retention ?? "preserve_durable"}
              onChange={() => update("raw_retention", "preserve_durable")}
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            >
              <option value="preserve_durable">Preserve durable history</option>
            </select>
          </Field>
          <div className="hidden md:block" />
          <Field label="Summarizer system prompt">
            <textarea
              value={config.summarizer_system_prompt}
              onChange={(event) => update("summarizer_system_prompt", event.target.value)}
              rows={5}
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-xs text-fg outline-none transition focus:border-fg"
            />
          </Field>
          <Field label="Summarizer user prompt">
            <textarea
              value={config.summarizer_user_prompt}
              onChange={(event) => update("summarizer_user_prompt", event.target.value)}
              rows={5}
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-xs text-fg outline-none transition focus:border-fg"
            />
            <p className="mt-1 text-xs text-fg-faint">
              Include <span className="font-mono">{"{messages}"}</span> where the transcript should
              be inserted.
            </p>
          </Field>
        </div>
      ) : (
        <p className="mt-4 text-sm text-fg-soft">
          Uses the built-in background summarizer configuration when context policy enables
          autocompaction.
        </p>
      )}
    </section>
  );
}
