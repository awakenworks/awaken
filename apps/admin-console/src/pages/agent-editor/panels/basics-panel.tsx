import { useTranslation } from "react-i18next";
import { type AgentSpec, type Capabilities } from "@/lib/config-api";
import { Field } from "@/components/form-components";
import {
  REASONING_EFFORT_PRESETS,
  reasoningEffortMode,
  reasoningEffortValue,
} from "@/lib/reasoning-effort";

export function BasicsPanel({
  spec,
  capabilities,
  isNew,
  updateField,
  reasoningMode,
  errors,
  canResetFields,
  overriddenFields,
  onResetField,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  isNew: boolean;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
  reasoningMode: ReturnType<typeof reasoningEffortMode>;
  errors?: Partial<Record<"id" | "model_id", string>>;
  canResetFields?: boolean;
  overriddenFields?: Set<string>;
  onResetField?: (field: string) => void;
}) {
  const { t } = useTranslation();
  const fieldResetProps = (field: string) => {
    if (!canResetFields || !overriddenFields?.has(field) || !onResetField) {
      return {};
    }
    return {
      overridden: true,
      onReset: () => onResetField(field),
      resetLabel: t("agents.resetOverrideField"),
    } as const;
  };
  return (
    <section className="rounded-md border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">Basics</h3>
      <div className="mt-4 grid gap-4 md:grid-cols-2">
        <Field label="Agent ID" required={isNew} error={errors?.id}>
          <input
            type="text"
            value={spec.id}
            disabled={!isNew}
            aria-invalid={Boolean(errors?.id)}
            onChange={(event) => updateField("id", event.target.value)}
            className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong disabled:bg-muted disabled:text-fg-soft aria-[invalid=true]:border-tone-error"
          />
        </Field>
        <Field label="Model" required error={errors?.model_id} {...fieldResetProps("model_id")}>
          <select
            value={String(spec.model_id ?? "")}
            aria-invalid={Boolean(errors?.model_id)}
            onChange={(event) => updateField("model_id", event.target.value)}
            className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong aria-[invalid=true]:border-tone-error"
          >
            <option value="">Select a model</option>
            {(capabilities?.models ?? []).map((model) => (
              <option key={model.id} value={model.id}>
                {model.id} ({model.upstream_model})
              </option>
            ))}
          </select>
        </Field>
        <Field label="Max rounds" {...fieldResetProps("max_rounds")}>
          <input
            type="number"
            min={1}
            value={Number(spec.max_rounds ?? 16)}
            onChange={(event) => updateField("max_rounds", Number(event.target.value) || 16)}
            className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
          />
        </Field>
        <Field label="Max continuation retries" {...fieldResetProps("max_continuation_retries")}>
          <input
            type="number"
            min={0}
            value={Number(spec.max_continuation_retries ?? 2)}
            onChange={(event) =>
              updateField("max_continuation_retries", Number(event.target.value) || 0)
            }
            className="w-full rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
          />
        </Field>
        <Field label="Reasoning effort">
          <div className="flex flex-wrap items-center gap-2">
            <select
              value={
                reasoningMode.kind === "default"
                  ? "__default__"
                  : reasoningMode.kind === "preset"
                    ? reasoningMode.value
                    : "__custom__"
              }
              onChange={(event) => {
                const choice = event.target.value;
                if (choice === "__default__") {
                  updateField(
                    "reasoning_effort",
                    reasoningEffortValue({ kind: "default" }) as string | number | null | undefined,
                  );
                  return;
                }
                if (choice === "__custom__") {
                  updateField(
                    "reasoning_effort",
                    reasoningEffortValue({
                      kind: "custom",
                      value: reasoningMode.kind === "custom" ? reasoningMode.value : "",
                    }) as string | number | null | undefined,
                  );
                  return;
                }
                updateField(
                  "reasoning_effort",
                  reasoningEffortValue({
                    kind: "preset",
                    value: choice as (typeof REASONING_EFFORT_PRESETS)[number],
                  }) as string | number | null | undefined,
                );
              }}
              className="rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
            >
              <option value="__default__">Provider default</option>
              {REASONING_EFFORT_PRESETS.map((preset) => (
                <option key={preset} value={preset}>
                  {preset}
                </option>
              ))}
              <option value="__custom__">Custom…</option>
            </select>
            {reasoningMode.kind === "custom" ? (
              <input
                type="text"
                value={reasoningMode.value}
                onChange={(event) =>
                  updateField(
                    "reasoning_effort",
                    reasoningEffortValue({
                      kind: "custom",
                      value: event.target.value,
                    }) as string | number | null | undefined,
                  )
                }
                placeholder="e.g. 1, 2, ultra"
                className="w-32 rounded-xl border border-line-strong px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
              />
            ) : null}
          </div>
        </Field>
      </div>

      <div className="mt-4">
        <Field label="System prompt" {...fieldResetProps("system_prompt")}>
          <textarea
            value={String(spec.system_prompt ?? "")}
            onChange={(event) => updateField("system_prompt", event.target.value)}
            rows={8}
            className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg-strong outline-none transition focus:border-line-strong"
          />
        </Field>
      </div>
    </section>
  );
}
