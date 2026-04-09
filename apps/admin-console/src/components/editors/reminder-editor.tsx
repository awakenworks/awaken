import { useEffect, useState } from "react";
import {
  type ReminderConfigDraft,
  type ReminderFieldConfig,
  type ReminderMode,
  createReminderField,
  createReminderRule,
  moveItem,
  normalizeReminderConfig,
  serializeReminderConfig,
} from "@/lib/plugin-config";
import {
  ChoiceGrid,
  EmptyState,
  Field,
  MetricCard,
  SectionLabel,
} from "@/components/form-components";

export function ReminderConfigEditor({
  value,
  onChange,
}: {
  value: unknown;
  onChange: (value: unknown) => void;
}) {
  const config = normalizeReminderConfig(value);
  const [activeRuleIndex, setActiveRuleIndex] = useState<number | null>(
    config.rules.length > 0 ? 0 : null,
  );

  useEffect(() => {
    if (config.rules.length === 0) {
      if (activeRuleIndex !== null) {
        setActiveRuleIndex(null);
      }
      return;
    }

    if (activeRuleIndex === null || activeRuleIndex >= config.rules.length) {
      setActiveRuleIndex(0);
    }
  }, [activeRuleIndex, config.rules.length]);

  function update(nextConfig: ReminderConfigDraft) {
    onChange(serializeReminderConfig(nextConfig));
  }

  function updateRule(index: number, nextRule: ReminderConfigDraft["rules"][number]) {
    update({
      ...config,
      rules: config.rules.map((rule, currentIndex) =>
        currentIndex === index ? nextRule : rule,
      ),
    });
  }

  function addRule() {
    const nextRules = [...config.rules, createReminderRule()];
    setActiveRuleIndex(nextRules.length - 1);
    update({ ...config, rules: nextRules });
  }

  function removeRule(index: number) {
    const nextRules = config.rules.filter((_, currentIndex) => currentIndex !== index);
    setActiveRuleIndex((current) => {
      if (nextRules.length === 0) {
        return null;
      }
      if (current === null) {
        return 0;
      }
      if (current > index) {
        return current - 1;
      }
      return Math.min(current, nextRules.length - 1);
    });
    update({ ...config, rules: nextRules });
  }

  function moveRule(index: number, direction: -1 | 1) {
    const targetIndex = index + direction;
    if (targetIndex < 0 || targetIndex >= config.rules.length) {
      return;
    }
    setActiveRuleIndex(targetIndex);
    update({
      ...config,
      rules: moveItem(config.rules, index, targetIndex),
    });
  }

  const activeRule =
    activeRuleIndex === null ? null : config.rules[activeRuleIndex] ?? null;

  return (
    <div className="space-y-5">
      <div className="grid gap-4 xl:grid-cols-[18rem,minmax(0,1fr)]">
        <div className="rounded-2xl border border-slate-200 bg-slate-50 p-4">
          <SectionLabel label="Reminder Summary" />
          <div className="mt-3 grid gap-3 sm:grid-cols-2 xl:grid-cols-1">
            <MetricCard
              label="Rules"
              value={`${config.rules.length}`}
              detail="Evaluated in order after tool execution."
            />
            <MetricCard
              label="Targets"
              value={summarizeReminderTargets(config)}
              detail="Where matching reminders are injected."
            />
          </div>
        </div>

        <div className="rounded-2xl border border-slate-200 bg-white p-4">
          <SectionLabel label="How Reminder Rules Work" />
          <p className="mt-2 text-sm leading-6 text-slate-500">
            Each rule watches a tool result, then injects a contextual reminder
            into the selected target. Use narrow tool patterns and specific
            output matchers so reminders stay relevant.
          </p>
          <div className="mt-4 grid gap-3 md:grid-cols-3">
            <MetricCard
              label="Match source"
              value="Tool output"
              detail="Status, text, or structured fields."
            />
            <MetricCard
              label="Injection"
              value="Context message"
              detail="System, suffix, session, or conversation."
            />
            <MetricCard
              label="Cooldown"
              value="Per rule"
              detail="Avoid repeating the same reminder every turn."
            />
          </div>
        </div>
      </div>

      <div className="grid gap-5 xl:grid-cols-[18rem,minmax(0,1fr)]">
        <aside className="space-y-3 rounded-2xl border border-slate-200 bg-slate-50 p-4">
          <div className="flex items-start justify-between gap-3">
            <div>
              <SectionLabel label="Reminder Rules" />
              <p className="mt-2 text-sm leading-6 text-slate-500">
                Each rule couples a tool-output matcher with the reminder that
                should be injected.
              </p>
            </div>
            <button
              type="button"
              onClick={addRule}
              className="rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm font-medium text-slate-700 transition hover:border-slate-400 hover:bg-slate-100"
            >
              Add reminder
            </button>
          </div>

          {config.rules.length === 0 ? (
            <EmptyState message="No reminder rules configured." compact />
          ) : (
            <div className="space-y-2">
              {config.rules.map((rule, index) => {
                const isActive = index === activeRuleIndex;
                return (
                  <button
                    key={`reminder-rule-${index}`}
                    type="button"
                    onClick={() => setActiveRuleIndex(index)}
                    className={[
                      "w-full rounded-2xl border px-4 py-3 text-left transition",
                      isActive
                        ? "border-slate-900 bg-slate-900 text-white shadow-sm"
                        : "border-slate-200 bg-white text-slate-900 hover:border-slate-300",
                    ].join(" ")}
                  >
                    <div className="flex items-center justify-between gap-3">
                      <div className="text-sm font-semibold">
                        {rule.name.trim() || `Reminder ${index + 1}`}
                      </div>
                      <span
                        className={[
                          "rounded-full px-2 py-0.5 text-[11px] font-medium",
                          isActive ? "bg-slate-700 text-slate-100" : "bg-cyan-100 text-cyan-700",
                        ].join(" ")}
                      >
                        {reminderModeShortLabel(rule.mode)}
                      </span>
                    </div>
                    <div
                      className={[
                        "mt-2 text-xs",
                        isActive ? "text-slate-300" : "text-slate-500",
                      ].join(" ")}
                    >
                      {rule.tool.trim() ? `Tool: ${rule.tool}` : "Applies to any tool"}
                    </div>
                    <div
                      className={[
                        "mt-2 text-xs font-medium",
                        isActive ? "text-slate-300" : "text-slate-500",
                      ].join(" ")}
                    >
                      Target: {reminderTargetLabel(rule.target)}
                    </div>
                  </button>
                );
              })}
            </div>
          )}
        </aside>

        {activeRule ? (
          <div className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div>
                <h6 className="text-lg font-semibold text-slate-950">
                  {activeRule.name.trim() || `Reminder ${activeRuleIndex! + 1}`}
                </h6>
                <p className="mt-1 text-sm leading-6 text-slate-500">
                  Model the trigger first, then define the reminder payload and
                  where it should be injected.
                </p>
              </div>
              <div className="flex flex-wrap gap-2">
                <button
                  type="button"
                  disabled={activeRuleIndex === 0}
                  onClick={() => moveRule(activeRuleIndex!, -1)}
                  className="rounded-xl border border-slate-300 px-3 py-2 text-sm font-medium text-slate-700 transition hover:border-slate-400 hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-50"
                >
                  Move up
                </button>
                <button
                  type="button"
                  disabled={activeRuleIndex === config.rules.length - 1}
                  onClick={() => moveRule(activeRuleIndex!, 1)}
                  className="rounded-xl border border-slate-300 px-3 py-2 text-sm font-medium text-slate-700 transition hover:border-slate-400 hover:bg-slate-50 disabled:cursor-not-allowed disabled:opacity-50"
                >
                  Move down
                </button>
                <button
                  type="button"
                  onClick={() => removeRule(activeRuleIndex!)}
                  className="rounded-xl border border-rose-200 px-3 py-2 text-sm font-medium text-rose-700 transition hover:bg-rose-50"
                >
                  Remove
                </button>
              </div>
            </div>

            <div className="mt-5 grid gap-4 lg:grid-cols-2">
              <Field label="Rule name">
                <input
                  type="text"
                  value={activeRule.name}
                  onChange={(event) =>
                    updateRule(activeRuleIndex!, {
                      ...activeRule,
                      name: event.target.value,
                    })
                  }
                  placeholder="weather-travel-hint"
                  className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                />
              </Field>
              <Field label="Tool pattern">
                <input
                  type="text"
                  value={activeRule.tool}
                  onChange={(event) =>
                    updateRule(activeRuleIndex!, {
                      ...activeRule,
                      tool: event.target.value,
                    })
                  }
                  placeholder="get_weather"
                  className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                />
              </Field>
            </div>

            <div className="mt-5 rounded-2xl border border-slate-200 bg-slate-50 p-4">
              <SectionLabel label="Trigger" />
              <p className="mt-2 text-sm leading-6 text-slate-500">
                Decide which part of the tool result must match before the
                reminder fires.
              </p>

              <div className="mt-4">
                <ChoiceGrid
                  value={activeRule.mode}
                  onChange={(nextValue) =>
                    updateRule(activeRuleIndex!, {
                      ...activeRule,
                      mode: nextValue,
                    })
                  }
                  columns="md:grid-cols-2 xl:grid-cols-3"
                  options={[
                    {
                      value: "any",
                      label: "Any output",
                      description: "Every result from the matching tool.",
                    },
                    {
                      value: "status",
                      label: "Status only",
                      description: "Match success, error, pending, or any.",
                    },
                    {
                      value: "content_text",
                      label: "Content text",
                      description: "Match text/glob content from the result.",
                    },
                    {
                      value: "content_fields",
                      label: "Content fields",
                      description: "Match structured fields in JSON content.",
                    },
                    {
                      value: "status_and_text",
                      label: "Status + text",
                      description: "Require both status and text to match.",
                    },
                    {
                      value: "status_and_fields",
                      label: "Status + fields",
                      description: "Require both status and field matchers.",
                    },
                  ]}
                />
              </div>

              {activeRule.mode !== "any" &&
              activeRule.mode !== "content_text" &&
              activeRule.mode !== "content_fields" ? (
                <div className="mt-4">
                  <SectionLabel label="Status Filter" />
                  <div className="mt-3">
                    <ChoiceGrid
                      value={activeRule.status}
                      onChange={(nextValue) =>
                        updateRule(activeRuleIndex!, {
                          ...activeRule,
                          status: nextValue,
                        })
                      }
                      columns="md:grid-cols-4"
                      options={[
                        {
                          value: "success",
                          label: "Success",
                          description: "Tool completed successfully.",
                        },
                        {
                          value: "error",
                          label: "Error",
                          description: "Tool returned an error.",
                        },
                        {
                          value: "pending",
                          label: "Pending",
                          description: "Tool result is still pending.",
                        },
                        {
                          value: "any",
                          label: "Any",
                          description: "Ignore the status code.",
                        },
                      ]}
                    />
                  </div>
                </div>
              ) : null}

              {activeRule.mode === "content_text" ||
              activeRule.mode === "status_and_text" ? (
                <div className="mt-4">
                  <Field label="Content text matcher">
                    <input
                      type="text"
                      value={activeRule.text}
                      onChange={(event) =>
                        updateRule(activeRuleIndex!, {
                          ...activeRule,
                          text: event.target.value,
                        })
                      }
                      placeholder="*permission denied*"
                      className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                    />
                  </Field>
                </div>
              ) : null}

              {activeRule.mode === "content_fields" ||
              activeRule.mode === "status_and_fields" ? (
                <div className="mt-4">
                  <div className="mb-2 flex items-center justify-between gap-4">
                    <SectionLabel label="Field Matchers" />
                    <button
                      type="button"
                      onClick={() =>
                        updateRule(activeRuleIndex!, {
                          ...activeRule,
                          fields: [...activeRule.fields, createReminderField()],
                        })
                      }
                      className="rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm font-medium text-slate-700 transition hover:border-slate-400 hover:bg-slate-100"
                    >
                      Add field condition
                    </button>
                  </div>
                  {activeRule.fields.length === 0 ? (
                    <EmptyState message="No field conditions configured yet." compact />
                  ) : (
                    <div className="space-y-3">
                      {activeRule.fields.map((field, fieldIndex) => (
                        <div
                          key={`reminder-field-${activeRuleIndex}-${fieldIndex}`}
                          className="grid gap-3 rounded-2xl border border-slate-200 bg-white p-3 lg:grid-cols-[minmax(0,1.1fr),12rem,minmax(0,1fr),auto]"
                        >
                          <Field label="Path">
                            <input
                              type="text"
                              value={field.path}
                              onChange={(event) =>
                                updateRule(activeRuleIndex!, {
                                  ...activeRule,
                                  fields: activeRule.fields.map((currentField, currentIndex) =>
                                    currentIndex === fieldIndex
                                      ? { ...field, path: event.target.value }
                                      : currentField,
                                  ),
                                })
                              }
                              placeholder="error.code"
                              className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                            />
                          </Field>
                          <Field label="Operation">
                            <select
                              value={field.op}
                              onChange={(event) =>
                                updateRule(activeRuleIndex!, {
                                  ...activeRule,
                                  fields: activeRule.fields.map((currentField, currentIndex) =>
                                    currentIndex === fieldIndex
                                      ? {
                                          ...field,
                                          op: event.target.value as ReminderFieldConfig["op"],
                                        }
                                      : currentField,
                                  ),
                                })
                              }
                              className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                            >
                              <option value="glob">Glob</option>
                              <option value="exact">Exact</option>
                              <option value="regex">Regex</option>
                              <option value="not_glob">Not glob</option>
                              <option value="not_exact">Not exact</option>
                              <option value="not_regex">Not regex</option>
                            </select>
                          </Field>
                          <Field label="Value">
                            <input
                              type="text"
                              value={field.value}
                              onChange={(event) =>
                                updateRule(activeRuleIndex!, {
                                  ...activeRule,
                                  fields: activeRule.fields.map((currentField, currentIndex) =>
                                    currentIndex === fieldIndex
                                      ? { ...field, value: event.target.value }
                                      : currentField,
                                  ),
                                })
                              }
                              placeholder="403"
                              className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                            />
                          </Field>
                          <div className="flex items-end">
                            <button
                              type="button"
                              onClick={() =>
                                updateRule(activeRuleIndex!, {
                                  ...activeRule,
                                  fields: activeRule.fields.filter(
                                    (_, currentIndex) => currentIndex !== fieldIndex,
                                  ),
                                })
                              }
                              className="rounded-xl border border-rose-200 px-3 py-2 text-sm font-medium text-rose-700 transition hover:bg-rose-50"
                            >
                              Remove
                            </button>
                          </div>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              ) : null}
            </div>

            <div className="mt-5 rounded-2xl border border-slate-200 bg-white">
              <div className="border-b border-slate-200 px-4 py-4">
                <SectionLabel label="Injected Reminder" />
                <p className="mt-2 text-sm leading-6 text-slate-500">
                  Configure where the reminder goes and what message should be
                  added when this rule matches.
                </p>
              </div>

              <div className="px-4 py-4">
                <ChoiceGrid
                  value={activeRule.target}
                  onChange={(nextValue) =>
                    updateRule(activeRuleIndex!, {
                      ...activeRule,
                      target: nextValue,
                    })
                  }
                  columns="md:grid-cols-2 xl:grid-cols-4"
                  options={[
                    {
                      value: "system",
                      label: "System",
                      description: "Inject into the system prompt chain.",
                    },
                    {
                      value: "suffix_system",
                      label: "Suffix system",
                      description: "Append after the system prompt.",
                    },
                    {
                      value: "session",
                      label: "Session",
                      description: "Persist in session-level context.",
                    },
                    {
                      value: "conversation",
                      label: "Conversation",
                      description: "Add to active conversation context.",
                    },
                  ]}
                />

                <div className="mt-4 grid gap-4 lg:grid-cols-[minmax(0,1fr),14rem]">
                  <Field label="Reminder content">
                    <textarea
                      value={activeRule.content}
                      onChange={(event) =>
                        updateRule(activeRuleIndex!, {
                          ...activeRule,
                          content: event.target.value,
                        })
                      }
                      rows={5}
                      className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                    />
                  </Field>
                  <Field label="Cooldown turns">
                    <input
                      type="number"
                      min={0}
                      value={activeRule.cooldown_turns}
                      onChange={(event) =>
                        updateRule(activeRuleIndex!, {
                          ...activeRule,
                          cooldown_turns: Number(event.target.value) || 0,
                        })
                      }
                      className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                    />
                  </Field>
                </div>
              </div>
            </div>
          </div>
        ) : (
          <EmptyState message="Add a reminder rule to describe the trigger and the reminder message to inject." />
        )}
      </div>
    </div>
  );
}

function reminderModeShortLabel(mode: ReminderMode): string {
  switch (mode) {
    case "any":
      return "Any";
    case "status":
      return "Status";
    case "content_text":
      return "Text";
    case "content_fields":
      return "Fields";
    case "status_and_text":
      return "Status + text";
    case "status_and_fields":
      return "Status + fields";
  }
}

function reminderTargetLabel(target: ReminderConfigDraft["rules"][number]["target"]): string {
  switch (target) {
    case "system":
      return "System";
    case "suffix_system":
      return "Suffix system";
    case "session":
      return "Session";
    case "conversation":
      return "Conversation";
  }
}

function summarizeReminderTargets(config: ReminderConfigDraft): string {
  const targets = Array.from(new Set(config.rules.map((rule) => reminderTargetLabel(rule.target))));
  if (targets.length === 0) {
    return "None";
  }
  if (targets.length <= 2) {
    return targets.join(", ");
  }
  return `${targets.slice(0, 2).join(", ")} +${targets.length - 2}`;
}
