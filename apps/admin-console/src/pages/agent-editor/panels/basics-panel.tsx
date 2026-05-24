import { useEffect, useState } from "react";
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
  const [maxRoundsDraft, setMaxRoundsDraft] = useState(() => String(spec.max_rounds ?? 16));
  const [maxRetriesDraft, setMaxRetriesDraft] = useState(() =>
    String(spec.max_continuation_retries ?? 2),
  );

  useEffect(() => {
    setMaxRoundsDraft(String(spec.max_rounds ?? 16));
  }, [spec.max_rounds]);

  useEffect(() => {
    setMaxRetriesDraft(String(spec.max_continuation_retries ?? 2));
  }, [spec.max_continuation_retries]);

  function commitNumberDraft(
    rawValue: string,
    min: number,
    fallback: number,
    onCommit: (value: number) => void,
    onRevert: (value: string) => void,
  ) {
    const trimmed = rawValue.trim();
    if (!trimmed) {
      onRevert(String(fallback));
      onCommit(fallback);
      return;
    }
    const parsed = Number(trimmed);
    const next = Number.isFinite(parsed) ? Math.max(min, Math.trunc(parsed)) : fallback;
    onRevert(String(next));
    onCommit(next);
  }

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
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">{t("editor.tabs.basics")}</h3>
      <div className="mt-4 grid gap-4 md:grid-cols-2">
        <Field label={t("editor.fields.agentId")} required={isNew} error={errors?.id}>
          <input
            type="text"
            value={spec.id}
            disabled={!isNew}
            aria-invalid={Boolean(errors?.id)}
            onChange={(event) => updateField("id", event.target.value)}
            className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg disabled:bg-muted disabled:text-fg-soft aria-[invalid=true]:border-tone-error"
          />
        </Field>
        <Field
          label={t("editor.fields.model")}
          required
          error={errors?.model_id}
          {...fieldResetProps("model_id")}
        >
          <select
            value={String(spec.model_id ?? "")}
            aria-invalid={Boolean(errors?.model_id)}
            onChange={(event) => updateField("model_id", event.target.value)}
            className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg aria-[invalid=true]:border-tone-error"
          >
            <option value="">{t("editor.fields.selectModel")}</option>
            {(capabilities?.models ?? []).map((model) => {
              // Surface context-window when published so authors can pick a
              // model with sufficient headroom for the agent's prompts.
              const ctx = model.context_window
                ? ` · ${
                    model.context_window >= 1_000
                      ? `${Math.round(model.context_window / 1_000)}K ctx`
                      : `${model.context_window} ctx`
                  }`
                : "";
              return (
                <option key={model.id} value={model.id}>
                  {model.id} ({model.upstream_model}){ctx}
                </option>
              );
            })}
          </select>
        </Field>
        <Field label={t("editor.fields.maxRounds")} {...fieldResetProps("max_rounds")}>
          <input
            type="number"
            min={1}
            value={maxRoundsDraft}
            onChange={(event) => setMaxRoundsDraft(event.target.value)}
            onBlur={() =>
              commitNumberDraft(
                maxRoundsDraft,
                1,
                Number(spec.max_rounds ?? 16),
                (value) => updateField("max_rounds", value),
                setMaxRoundsDraft,
              )
            }
            className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
          />
        </Field>
        <Field
          label={t("editor.fields.maxRetries")}
          {...fieldResetProps("max_continuation_retries")}
        >
          <input
            type="number"
            min={0}
            value={maxRetriesDraft}
            onChange={(event) => setMaxRetriesDraft(event.target.value)}
            onBlur={() =>
              commitNumberDraft(
                maxRetriesDraft,
                0,
                Number(spec.max_continuation_retries ?? 2),
                (value) => updateField("max_continuation_retries", value),
                setMaxRetriesDraft,
              )
            }
            className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
          />
        </Field>
        <Field label={t("editor.fields.reasoningEffort")}>
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
              className="rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
            >
              <option value="__default__">{t("editor.fields.providerDefault")}</option>
              {REASONING_EFFORT_PRESETS.map((preset) => (
                <option key={preset} value={preset}>
                  {preset}
                </option>
              ))}
              <option value="__custom__">{t("editor.fields.custom")}</option>
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
                placeholder={t("editor.fields.reasoningEffortPlaceholder")}
                className="w-32 rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
              />
            ) : null}
          </div>
        </Field>
      </div>

      <div className="mt-4">
        <Field label={t("editor.fields.systemPrompt")} {...fieldResetProps("system_prompt")}>
          <textarea
            value={String(spec.system_prompt ?? "")}
            onChange={(event) => updateField("system_prompt", event.target.value)}
            rows={8}
            className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg outline-none transition focus:border-fg"
          />
        </Field>
      </div>
    </section>
  );
}
