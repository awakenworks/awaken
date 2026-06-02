import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import type { AgentBackendSpec, AgentSpec, Capabilities } from "@/lib/config-api";
import { Field } from "@/components/form-components";
import {
  REASONING_EFFORT_PRESETS,
  reasoningEffortMode,
  reasoningEffortValue,
} from "@/lib/reasoning-effort";

export const AWAKEN_BACKEND_KIND = "awaken";

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value && typeof value === "object" && !Array.isArray(value));
}

function backendConfigRecord(backend: AgentBackendSpec | undefined): Record<string, unknown> {
  return isRecord(backend?.config) ? backend.config : {};
}

export function currentBackend(spec: AgentSpec): AgentBackendSpec {
  if (spec.backend?.kind) {
    return {
      kind: spec.backend.kind,
      version: spec.backend.version ?? 1,
      config:
        spec.backend.kind === AWAKEN_BACKEND_KIND
          ? {
              ...backendConfigRecord(spec.backend),
              model_id: spec.model_id ?? "",
              system_prompt: spec.system_prompt ?? "",
              max_rounds: spec.max_rounds ?? 16,
            }
          : backendConfigRecord(spec.backend),
    };
  }
  return {
    kind: AWAKEN_BACKEND_KIND,
    version: 1,
    config: {
      model_id: spec.model_id ?? "",
      system_prompt: spec.system_prompt ?? "",
      max_rounds: spec.max_rounds ?? 16,
    },
  };
}

export function backendDefaultConfig(
  capabilities: Capabilities | null,
  kind: string,
  spec: AgentSpec,
): Record<string, unknown> {
  if (kind === AWAKEN_BACKEND_KIND) {
    return {
      ...asRecord(
        capabilities?.backends?.find((candidate) => candidate.kind === kind)?.default_config,
      ),
      model_id: spec.model_id ?? "",
      system_prompt: spec.system_prompt ?? "",
      max_rounds: spec.max_rounds ?? 16,
    };
  }
  return asRecord(
    capabilities?.backends?.find((candidate) => candidate.kind === kind)?.default_config,
  );
}

function asRecord(value: unknown): Record<string, unknown> {
  return isRecord(value) ? { ...value } : {};
}

export function applyBackendConfig(
  spec: AgentSpec,
  kind: string,
  version: number,
  config: Record<string, unknown>,
): AgentSpec {
  const backend = { kind, version, config };
  if (kind !== AWAKEN_BACKEND_KIND) {
    return {
      ...spec,
      backend,
      endpoint: undefined,
      model_id: "",
      system_prompt: "",
    };
  }

  const modelId = typeof config.model_id === "string" ? config.model_id : spec.model_id;
  const systemPrompt =
    typeof config.system_prompt === "string" ? config.system_prompt : spec.system_prompt;
  const maxRounds =
    typeof config.max_rounds === "number" && Number.isFinite(config.max_rounds)
      ? Math.max(1, Math.floor(config.max_rounds))
      : (spec.max_rounds ?? 16);
  return {
    ...spec,
    backend,
    endpoint: undefined,
    model_id: modelId,
    system_prompt: systemPrompt,
    max_rounds: maxRounds,
  };
}

export function syncAwakenBackend(spec: AgentSpec): AgentSpec {
  if (currentBackend(spec).kind !== AWAKEN_BACKEND_KIND) {
    return spec;
  }
  return {
    ...spec,
    backend: {
      kind: AWAKEN_BACKEND_KIND,
      version: spec.backend?.version ?? 1,
      config: {
        ...backendConfigRecord(spec.backend),
        model_id: spec.model_id ?? "",
        system_prompt: spec.system_prompt ?? "",
        max_rounds: spec.max_rounds ?? 16,
      },
    },
  };
}

export function BasicsPanel({
  spec,
  capabilities,
  isNew,
  updateField,
  updateBackend,
  errors,
  canResetFields,
  overriddenFields,
  onResetField,
  onCloneFrom,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
  isNew: boolean;
  updateField: <K extends keyof AgentSpec>(key: K, value: AgentSpec[K]) => void;
  updateBackend: (kind: string, config: Record<string, unknown>) => void;
  errors?: Partial<Record<"id" | "model_id", string>>;
  canResetFields?: boolean;
  overriddenFields?: Set<string>;
  onResetField?: (field: string) => void;
  onCloneFrom?: (sourceId: string) => void;
}) {
  const { t } = useTranslation();
  const reasoningMode = reasoningEffortMode(spec.reasoning_effort);
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
  const cloneOptions = isNew
    ? (capabilities?.agents ?? []).filter((agentId) => agentId !== spec.id)
    : [];
  const backend = currentBackend(spec);
  const backendOptions =
    capabilities?.backends && capabilities.backends.length > 0
      ? capabilities.backends
      : [
          {
            kind: AWAKEN_BACKEND_KIND,
            version: 1,
            display_name: "Awaken",
            default_config: {
              model_id: spec.model_id ?? "",
              system_prompt: spec.system_prompt ?? "",
              max_rounds: spec.max_rounds ?? 16,
            },
          },
        ];
  const isAwakenBackend = backend.kind === AWAKEN_BACKEND_KIND;

  function parseIntegerInput(value: string, min: number): number | undefined {
    if (value.trim() === "") return undefined;
    const parsed = Number(value);
    if (!Number.isFinite(parsed) || parsed < min) return undefined;
    return Math.floor(parsed);
  }

  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">Basics</h3>
      {isNew && cloneOptions.length > 0 && onCloneFrom ? (
        <div className="mt-3 flex flex-wrap items-center gap-2 rounded-sm border border-dashed border-line bg-soft px-3 py-2 text-xs text-fg-soft">
          <span className="font-medium uppercase tracking-eyebrow text-[10px]">Start from</span>
          <select
            defaultValue=""
            onChange={(event) => {
              const next = event.target.value;
              if (next) {
                onCloneFrom(next);
                event.target.value = "";
              }
            }}
            className="rounded-sm border border-line-strong bg-surface px-2 py-1 text-xs text-fg outline-none transition focus:border-fg"
          >
            <option value="">Blank agent</option>
            {cloneOptions.map((agentId) => (
              <option key={agentId} value={agentId}>
                Clone from {agentId}
              </option>
            ))}
          </select>
          <span className="text-[10px] text-fg-faint">
            Pick an existing agent to copy its prompt / tools / plugins. You still pick a new id.
            <span className="ml-1">
              <span className="font-mono">endpoint</span> and{" "}
              <span className="font-mono">registry</span> are not copied — the clone is always a
              locally-defined agent.
            </span>
          </span>
        </div>
      ) : null}
      <div className="mt-4 grid gap-4 md:grid-cols-2">
        <Field label="Agent ID" required={isNew} error={errors?.id}>
          <input
            type="text"
            value={spec.id}
            disabled={!isNew}
            aria-invalid={Boolean(errors?.id)}
            onChange={(event) => updateField("id", event.target.value)}
            className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg disabled:bg-muted disabled:text-fg-soft aria-[invalid=true]:border-tone-error"
          />
        </Field>
        <Field label="Backend">
          <select
            value={backend.kind}
            onChange={(event) => {
              const kind = event.target.value;
              updateBackend(kind, backendDefaultConfig(capabilities, kind, spec));
            }}
            className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
          >
            {backendOptions.map((option) => (
              <option key={option.kind} value={option.kind}>
                {option.display_name ??
                  (option.kind === AWAKEN_BACKEND_KIND ? "Awaken" : option.kind)}
              </option>
            ))}
          </select>
        </Field>
        {isAwakenBackend ? (
          <>
            <div>
              <Field
                label="Model"
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
                  <option value="">Select a model</option>
                  {(capabilities?.models ?? []).map((model) => (
                    <option key={model.id} value={model.id}>
                      {model.id} — {model.provider_id} · {model.upstream_model}
                    </option>
                  ))}
                </select>
              </Field>
              <SelectedModelBadge spec={spec} capabilities={capabilities} />
            </div>
            <Field label="Max rounds" {...fieldResetProps("max_rounds")}>
              <input
                type="number"
                min={1}
                value={Number(spec.max_rounds ?? 16)}
                onChange={(event) => {
                  const next = parseIntegerInput(event.target.value, 1);
                  if (next !== undefined) updateField("max_rounds", next);
                }}
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
              />
            </Field>
            <Field
              label="Max continuation retries"
              {...fieldResetProps("max_continuation_retries")}
            >
              <input
                type="number"
                min={0}
                value={Number(spec.max_continuation_retries ?? 2)}
                onChange={(event) => {
                  const next = parseIntegerInput(event.target.value, 0);
                  if (next !== undefined) updateField("max_continuation_retries", next);
                }}
                className="w-full rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
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
                        reasoningEffortValue({ kind: "default" }) as
                          | string
                          | number
                          | null
                          | undefined,
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
                    className="w-32 rounded-sm border border-line-strong px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
                  />
                ) : null}
              </div>
            </Field>
          </>
        ) : null}
      </div>

      {isAwakenBackend ? (
        <div className="mt-4">
          <Field label="System prompt" {...fieldResetProps("system_prompt")}>
            <textarea
              value={String(spec.system_prompt ?? "")}
              onChange={(event) => updateField("system_prompt", event.target.value)}
              rows={8}
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg outline-none transition focus:border-fg"
            />
          </Field>
          <SystemPromptStats value={String(spec.system_prompt ?? "")} />
        </div>
      ) : null}
    </section>
  );
}

function SelectedModelBadge({
  spec,
  capabilities,
}: {
  spec: AgentSpec;
  capabilities: Capabilities | null;
}) {
  const selectedId = String(spec.model_id ?? "");
  const selected = (capabilities?.models ?? []).find((model) => model.id === selectedId);
  if (!selected) return null;
  return (
    <div
      aria-hidden="true"
      className="mt-1 flex flex-wrap items-center gap-2 text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft"
    >
      <span
        className="rounded-pill bg-muted px-2 py-0.5 text-fg-soft"
        title={`Resolved provider: ${selected.provider_id}`}
      >
        provider · {selected.provider_id}
      </span>
      <span
        className="rounded-pill bg-muted px-2 py-0.5 text-fg-soft"
        title={`Upstream model identifier sent to the provider: ${selected.upstream_model}`}
      >
        upstream · {selected.upstream_model}
      </span>
    </div>
  );
}

function SystemPromptStats({ value }: { value: string }) {
  const stats = useMemo(() => promptStats(value), [value]);
  return (
    <div
      aria-hidden="true"
      className="mt-1 flex flex-wrap items-center gap-3 text-[10px] font-medium uppercase tracking-eyebrow text-fg-soft"
    >
      <span>{stats.chars.toLocaleString()} chars</span>
      <span className="text-fg-faint">·</span>
      <span>{stats.lines.toLocaleString()} lines</span>
      <span className="text-fg-faint">·</span>
      <span title="Rough estimate (chars / 4) — actual tokens depend on the tokenizer">
        ~{stats.tokenEstimate.toLocaleString()} tokens
      </span>
    </div>
  );
}

function promptStats(value: string): { chars: number; lines: number; tokenEstimate: number } {
  if (value.length === 0) {
    return { chars: 0, lines: 0, tokenEstimate: 0 };
  }
  const chars = value.length;
  const lines = value.split("\n").length;
  const tokenEstimate = Math.ceil(chars / 4);
  return { chars, lines, tokenEstimate };
}
