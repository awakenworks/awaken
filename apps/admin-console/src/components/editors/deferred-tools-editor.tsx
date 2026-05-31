import {
  type AgentPriorConfig,
  type DeferredToolsConfig,
  type ToolLoadMode,
  createAgentPrior,
  createDeferredToolsRule,
  moveItem,
  normalizeDeferredToolsConfig,
  serializeDeferredToolsConfig,
} from "@/lib/plugin-config";
import { EmptyState, Field, Hint, SectionLabel } from "@/components/form-components";
import { ToolPatternHelp } from "@/components/tool-pattern-help";
import {
  EXACT_TOOL_ID_PLACEHOLDER,
  TOOL_ID_PATTERN_PLACEHOLDER,
} from "@/lib/tool-pattern-guidance";

export function DeferredToolsConfigEditor({
  value,
  onChange,
}: {
  value: unknown;
  onChange: (value: unknown) => void;
}) {
  const config = normalizeDeferredToolsConfig(value);

  function update(nextConfig: DeferredToolsConfig) {
    onChange(serializeDeferredToolsConfig(nextConfig));
  }

  function updateRule(index: number, patch: Partial<DeferredToolsConfig["rules"][number]>) {
    update({
      ...config,
      rules: config.rules.map((rule, currentIndex) =>
        currentIndex === index ? { ...rule, ...patch } : rule,
      ),
    });
  }

  function addRule() {
    update({
      ...config,
      rules: [...config.rules, createDeferredToolsRule()],
    });
  }

  function removeRule(index: number) {
    update({
      ...config,
      rules: config.rules.filter((_, currentIndex) => currentIndex !== index),
    });
  }

  function moveRule(index: number, direction: -1 | 1) {
    const target = index + direction;
    if (target < 0 || target >= config.rules.length) return;
    update({
      ...config,
      rules: moveItem(config.rules, index, target),
    });
  }

  function updatePrior(index: number, patch: Partial<AgentPriorConfig>) {
    update({
      ...config,
      agent_priors: config.agent_priors.map((prior, currentIndex) =>
        currentIndex === index ? { ...prior, ...patch } : prior,
      ),
    });
  }

  return (
    <div className="space-y-5">
      <div className="rounded-sm border border-line bg-soft px-4 py-3">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div>
            <SectionLabel label="Deferred Loading Policy" />
            <Hint>
              Deferred Tools keeps low-probability tools out of the initial model tool catalog, then
              promotes them when the conversation shows intent. This reduces prompt/tool overhead
              without removing the tools from the agent.
            </Hint>
          </div>
          <div className="flex flex-wrap gap-2 text-[11px] text-fg-soft">
            <span className="rounded-pill bg-muted px-2 py-0.5">
              {config.rules.length} rule{config.rules.length === 1 ? "" : "s"}
            </span>
            <span className="rounded-pill bg-muted px-2 py-0.5">
              default {loadModeLabel(config.default_mode)}
            </span>
          </div>
        </div>
        <div className="mt-3 grid gap-3 md:grid-cols-3">
          <Field label="Activation">
            <select
              value={config.enabled}
              onChange={(event) =>
                update({
                  ...config,
                  enabled: event.target.value as DeferredToolsConfig["enabled"],
                })
              }
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none focus:border-fg"
            >
              <option value="auto">Auto by savings</option>
              <option value="always">Always enable</option>
              <option value="disabled">Disable</option>
            </select>
          </Field>
          <Field label="Default load mode">
            <select
              value={config.default_mode}
              onChange={(event) =>
                update({
                  ...config,
                  default_mode: event.target.value as ToolLoadMode,
                })
              }
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none focus:border-fg"
            >
              <option value="deferred">Deferred</option>
              <option value="eager">Eager</option>
            </select>
          </Field>
          <Field label="Beta overhead tokens">
            <input
              type="number"
              min={0}
              value={config.beta_overhead}
              onChange={(event) =>
                update({
                  ...config,
                  beta_overhead: numericValue(event.currentTarget.value, 0),
                })
              }
              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none focus:border-fg"
            />
          </Field>
        </div>
      </div>

      <section className="rounded-sm border border-line bg-surface">
        <div className="flex flex-wrap items-start justify-between gap-3 border-b border-line px-4 py-3">
          <div>
            <SectionLabel label="Trigger Rules" />
            <p className="mt-1 text-xs leading-5 text-fg-soft">
              Each rule maps a tool id pattern to eager or deferred loading. First match wins.
            </p>
            <ToolPatternHelp kind="tool-id" className="mt-1 text-xs leading-5 text-fg-soft" />
          </div>
          <button
            type="button"
            onClick={addRule}
            className="rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm font-medium text-fg transition hover:bg-muted"
          >
            Add rule
          </button>
        </div>

        {config.rules.length === 0 ? (
          <div className="px-4 py-4">
            <EmptyState
              compact
              message="No explicit triggers. Tools fall back to the default load mode above."
            />
          </div>
        ) : (
          <div className="overflow-x-auto">
            <table className="min-w-full text-sm">
              <thead>
                <tr className="border-b border-line bg-soft text-left text-xs font-semibold uppercase tracking-eyebrow text-fg-soft">
                  <th className="w-12 px-3 py-2">Order</th>
                  <th className="min-w-[18rem] px-3 py-2">Tool pattern trigger</th>
                  <th className="w-44 px-3 py-2">Load mode</th>
                  <th className="w-40 px-3 py-2 text-right">Actions</th>
                </tr>
              </thead>
              <tbody>
                {config.rules.map((rule, index) => (
                  <tr key={`deferred-rule-${index}`} className="border-b border-line">
                    <td className="px-3 py-2 font-mono text-xs text-fg-soft">
                      #{String(index + 1).padStart(2, "0")}
                    </td>
                    <td className="px-3 py-2">
                      <input
                        type="text"
                        value={rule.tool}
                        onChange={(event) => updateRule(index, { tool: event.target.value })}
                        placeholder={TOOL_ID_PATTERN_PLACEHOLDER}
                        aria-label={`Deferred tool pattern ${index + 1}`}
                        className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg outline-none focus:border-fg"
                      />
                    </td>
                    <td className="px-3 py-2">
                      <select
                        value={rule.mode}
                        onChange={(event) =>
                          updateRule(index, { mode: event.target.value as ToolLoadMode })
                        }
                        aria-label={`Deferred load mode ${index + 1}`}
                        className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none focus:border-fg"
                      >
                        <option value="deferred">Deferred</option>
                        <option value="eager">Eager</option>
                      </select>
                    </td>
                    <td className="px-3 py-2">
                      <div className="flex justify-end gap-1">
                        <button
                          type="button"
                          disabled={index === 0}
                          onClick={() => moveRule(index, -1)}
                          className="rounded-sm border border-line bg-surface px-2 py-1 text-xs text-fg-soft hover:bg-soft disabled:opacity-40"
                        >
                          Up
                        </button>
                        <button
                          type="button"
                          disabled={index === config.rules.length - 1}
                          onClick={() => moveRule(index, 1)}
                          className="rounded-sm border border-line bg-surface px-2 py-1 text-xs text-fg-soft hover:bg-soft disabled:opacity-40"
                        >
                          Down
                        </button>
                        <button
                          type="button"
                          onClick={() => removeRule(index)}
                          className="rounded-sm border border-tone-error/30 px-2 py-1 text-xs text-tone-error hover:bg-tone-error/10"
                        >
                          Remove
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      <section className="rounded-sm border border-line bg-surface px-4 py-3">
        <SectionLabel label="Trigger Constraints" />
        <p className="mt-1 text-xs leading-5 text-fg-soft">
          These parameters tune when idle tools are re-deferred after use.{" "}
          <span className="font-mono">defer_after</span> is the cooldown-like constraint.
        </p>
        <div className="mt-3 grid gap-3 md:grid-cols-5">
          <NumberField
            label="Omega"
            value={config.disc_beta.omega}
            step="0.01"
            onChange={(omega) => update({ ...config, disc_beta: { ...config.disc_beta, omega } })}
          />
          <NumberField
            label="Prior n0"
            value={config.disc_beta.n0}
            step="0.1"
            onChange={(n0) => update({ ...config, disc_beta: { ...config.disc_beta, n0 } })}
          />
          <NumberField
            label="Defer after"
            value={config.disc_beta.defer_after}
            step="1"
            onChange={(defer_after) =>
              update({ ...config, disc_beta: { ...config.disc_beta, defer_after } })
            }
          />
          <NumberField
            label="Threshold"
            value={config.disc_beta.thresh_mult}
            step="0.1"
            onChange={(thresh_mult) =>
              update({ ...config, disc_beta: { ...config.disc_beta, thresh_mult } })
            }
          />
          <NumberField
            label="ToolSearch cost"
            value={config.disc_beta.gamma}
            step="100"
            onChange={(gamma) => update({ ...config, disc_beta: { ...config.disc_beta, gamma } })}
          />
        </div>
      </section>

      <details className="rounded-sm border border-line bg-surface">
        <summary className="cursor-pointer px-4 py-3 text-sm font-medium text-fg">
          Agent priors ({config.agent_priors.length})
        </summary>
        <div className="border-t border-line px-4 py-3">
          <p className="text-xs leading-5 text-fg-soft">
            Optional per-tool prior probabilities. Leave empty unless you have measured usage data.
          </p>
          <div className="mt-3 space-y-2">
            {config.agent_priors.map((prior, index) => (
              <div
                key={`agent-prior-${index}`}
                className="grid gap-2 md:grid-cols-[minmax(0,1fr),10rem,auto]"
              >
                <Field label="Tool ID">
                  <input
                    type="text"
                    value={prior.tool}
                    onChange={(event) => updatePrior(index, { tool: event.target.value })}
                    placeholder={EXACT_TOOL_ID_PLACEHOLDER}
                    className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg outline-none focus:border-fg"
                  />
                </Field>
                <Field label="Probability">
                  <input
                    type="number"
                    min={0}
                    max={1}
                    step={0.01}
                    value={prior.probability}
                    onChange={(event) =>
                      updatePrior(index, {
                        probability: numericValue(event.currentTarget.value, 0.01),
                      })
                    }
                    className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none focus:border-fg"
                  />
                </Field>
                <div className="flex items-end">
                  <button
                    type="button"
                    onClick={() =>
                      update({
                        ...config,
                        agent_priors: config.agent_priors.filter(
                          (_, currentIndex) => currentIndex !== index,
                        ),
                      })
                    }
                    className="rounded-sm border border-tone-error/30 px-3 py-2 text-sm font-medium text-tone-error hover:bg-tone-error/10"
                  >
                    Remove
                  </button>
                </div>
              </div>
            ))}
            <button
              type="button"
              onClick={() =>
                update({
                  ...config,
                  agent_priors: [...config.agent_priors, createAgentPrior()],
                })
              }
              className="rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm font-medium text-fg hover:bg-muted"
            >
              Add prior
            </button>
          </div>
        </div>
      </details>
    </div>
  );
}

function NumberField({
  label,
  value,
  step,
  onChange,
}: {
  label: string;
  value: number;
  step: string;
  onChange: (value: number) => void;
}) {
  return (
    <Field label={label}>
      <input
        type="number"
        step={step}
        value={value}
        onChange={(event) => onChange(numericValue(event.currentTarget.value, value))}
        className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none focus:border-fg"
      />
    </Field>
  );
}

function numericValue(value: string, fallback: number): number {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : fallback;
}

function loadModeLabel(mode: ToolLoadMode): string {
  return mode === "eager" ? "eager" : "deferred";
}
