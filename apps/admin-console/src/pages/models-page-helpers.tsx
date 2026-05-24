/**
 * Sub-components and pure helpers for `ModelsPage`. Extracted from
 * `models-page.tsx` so the page module stays under the repository's
 * per-file line cap. No state lives here — every component takes its
 * inputs as props and forwards user actions through callbacks.
 */
import type { ModelSpec, Modality } from "@/lib/config-api";
import { Field } from "@/components/form-components";

export const MODALITY_OPTIONS: readonly Modality[] = [
  "text",
  "image",
  "audio",
  "video",
  "pdf",
];

/** Client-side shape pre-filter for `knowledge_cutoff`: `YYYY-MM` or
 *  `YYYY-MM-DD` with month 01-12 and day 01-31. Cheap pre-check only — the
 *  server's `validate_knowledge_cutoff_format` is authoritative and also
 *  rejects calendar-invalid dates (e.g. 2026-02-31, non-leap 2026-02-29). */
export const KNOWLEDGE_CUTOFF_RE = /^\d{4}-(0[1-9]|1[0-2])(-(0[1-9]|[12]\d|3[01]))?$/;

/** Compact context-window display (`200K`, `1.2M`); fall back to localeString
 *  below 1K so the value stays human-readable. */
export function formatTokenCount(n: number): string {
  if (n >= 1_000_000) return `${Math.round(n / 100_000) / 10}M`;
  if (n >= 1_000) return `${Math.round(n / 100) / 10}K`;
  return n.toLocaleString();
}

/** Parse a numeric form input. Empty string => `undefined` (cleared);
 *  invalid => `NaN` so the validator can surface a field error. */
export function parseOptionalNumber(value: string): number | undefined {
  const trimmed = value.trim();
  if (trimmed === "") return undefined;
  return Number(trimmed);
}

/** Strip duplicates while preserving first-seen order — modalities are set
 *  semantics on the wire, so the form normalises before save. */
export function dedupModalities(list: Modality[]): Modality[] {
  return Array.from(new Set(list));
}

/** Toggle a single modality in `current` and return the next list. */
export function toggleModality(
  current: Modality[] | undefined,
  m: Modality,
): Modality[] {
  const set = new Set(current ?? []);
  if (set.has(m)) set.delete(m);
  else set.add(m);
  return Array.from(set);
}

export function PricingField({
  label,
  value,
  error,
  onChange,
}: {
  label: string;
  value: number | undefined;
  error: string | undefined;
  onChange: (parsed: number | undefined) => void;
}) {
  return (
    <Field label={label} error={error}>
      <div className="flex items-center gap-2">
        <input
          type="number"
          inputMode="decimal"
          min={0}
          step="any"
          value={value ?? ""}
          aria-invalid={Boolean(error)}
          onChange={(event) => onChange(parseOptionalNumber(event.target.value))}
          className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg aria-[invalid=true]:border-tone-error"
        />
        <span className="font-mono text-xs text-fg-faint">$ / Mtok</span>
      </div>
    </Field>
  );
}

export function ModalityChips({
  label,
  selected,
  onToggle,
}: {
  label: string;
  selected: Modality[];
  onToggle: (m: Modality) => void;
}) {
  const selectedSet = new Set(selected);
  return (
    <div className="block">
      <span className="mb-1.5 block text-sm font-medium text-fg-soft">{label}</span>
      <div role="group" aria-label={label} className="flex flex-wrap gap-2">
        {MODALITY_OPTIONS.map((m) => {
          const active = selectedSet.has(m);
          return (
            <button
              key={m}
              type="button"
              role="switch"
              aria-checked={active}
              onClick={() => onToggle(m)}
              className={[
                "inline-flex h-7 items-center rounded-pill border px-3 text-xs font-medium transition",
                active
                  ? "border-accent bg-accent text-accent-text"
                  : "border-line-strong bg-surface text-fg-soft hover:border-fg",
              ].join(" ")}
            >
              {m}
            </button>
          );
        })}
      </div>
    </div>
  );
}

export function CapabilitySummary({ model }: { model: ModelSpec }) {
  const ctx = model.context_window ? formatTokenCount(model.context_window) : null;
  const out = model.max_output_tokens ? formatTokenCount(model.max_output_tokens) : null;
  const inputModalities = model.modalities?.input ?? [];
  const outputModalities = model.modalities?.output ?? [];
  if (!ctx && !out && inputModalities.length === 0 && outputModalities.length === 0) {
    return <span className="text-fg-faint">—</span>;
  }
  return (
    <div className="flex flex-wrap items-center gap-2">
      {ctx && (
        <span className="rounded-pill border border-line bg-soft px-2 py-0.5 font-mono text-xs text-fg-soft">
          {ctx} ctx
        </span>
      )}
      {out && (
        <span className="rounded-pill border border-line bg-soft px-2 py-0.5 font-mono text-xs text-fg-soft">
          {out} out
        </span>
      )}
      {inputModalities.length > 0 && (
        <span
          className="rounded-pill border border-line bg-soft px-2 py-0.5 text-xs text-fg-soft"
          title={`Input: ${inputModalities.join(", ")}`}
        >
          in: {inputModalities.join("/")}
        </span>
      )}
      {outputModalities.length > 0 && (
        <span
          className="rounded-pill border border-line bg-soft px-2 py-0.5 text-xs text-fg-soft"
          title={`Output: ${outputModalities.join(", ")}`}
        >
          out: {outputModalities.join("/")}
        </span>
      )}
    </div>
  );
}

export type ModelErrorKey =
  | "id"
  | "provider_id"
  | "upstream_model"
  | "context_window"
  | "max_output_tokens"
  | "knowledge_cutoff"
  | "input_token_price_per_million_usd"
  | "output_token_price_per_million_usd";

export type ModelFieldErrors = Partial<Record<ModelErrorKey, string>>;

/** Mirror of the Rust `validate_model_spec` rules. Pure: no i18n or state. */
export function validateModelSpec(
  draft: ModelSpec,
  t: (key: string) => string,
): ModelFieldErrors {
  const next: ModelFieldErrors = {};
  if (!draft.id.trim()) next.id = t("validation.required");
  if (!draft.provider_id.trim()) next.provider_id = t("validation.required");
  if (!draft.upstream_model.trim()) next.upstream_model = t("validation.required");

  if (draft.context_window !== undefined) {
    if (
      !Number.isFinite(draft.context_window) ||
      !Number.isInteger(draft.context_window) ||
      draft.context_window < 1
    ) {
      next.context_window = t("models.validation.positiveInt");
    }
  }
  if (draft.max_output_tokens !== undefined) {
    if (
      !Number.isFinite(draft.max_output_tokens) ||
      !Number.isInteger(draft.max_output_tokens) ||
      draft.max_output_tokens < 1
    ) {
      next.max_output_tokens = t("models.validation.positiveInt");
    } else if (
      draft.context_window !== undefined &&
      !next.context_window &&
      draft.max_output_tokens > draft.context_window
    ) {
      next.max_output_tokens = t("models.validation.maxOutputExceedsContext");
    }
  }
  if (draft.knowledge_cutoff !== undefined && draft.knowledge_cutoff !== "") {
    if (!KNOWLEDGE_CUTOFF_RE.test(draft.knowledge_cutoff)) {
      next.knowledge_cutoff = t("models.validation.knowledgeCutoff");
    }
  }
  if (draft.input_token_price_per_million_usd !== undefined) {
    if (
      !Number.isFinite(draft.input_token_price_per_million_usd) ||
      draft.input_token_price_per_million_usd < 0
    ) {
      next.input_token_price_per_million_usd = t("models.validation.nonNegativeFinite");
    }
  }
  if (draft.output_token_price_per_million_usd !== undefined) {
    if (
      !Number.isFinite(draft.output_token_price_per_million_usd) ||
      draft.output_token_price_per_million_usd < 0
    ) {
      next.output_token_price_per_million_usd = t("models.validation.nonNegativeFinite");
    }
  }
  return next;
}

/** Normalise draft for save — drop empty knowledge_cutoff, dedup modalities,
 *  strip an empty `modalities` block entirely so the wire stays minimal. */
export function normalizeModelForSave(draft: ModelSpec): ModelSpec {
  const next: ModelSpec = { ...draft };
  if (next.knowledge_cutoff !== undefined && next.knowledge_cutoff.trim() === "") {
    delete next.knowledge_cutoff;
  }
  if (next.modalities) {
    const input = next.modalities.input ? dedupModalities(next.modalities.input) : undefined;
    const output = next.modalities.output ? dedupModalities(next.modalities.output) : undefined;
    const hasInput = input && input.length > 0;
    const hasOutput = output && output.length > 0;
    if (!hasInput && !hasOutput) {
      delete next.modalities;
    } else {
      next.modalities = {
        ...(hasInput ? { input } : {}),
        ...(hasOutput ? { output } : {}),
      };
    }
  }
  return next;
}
