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
import { ChoiceGrid, EmptyState, Field, SectionLabel } from "@/components/form-components";
import { ToolPatternHelp } from "@/components/tool-pattern-help";
import { TOOL_CALL_PATTERN_PLACEHOLDER } from "@/lib/tool-pattern-guidance";

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
      rules: config.rules.map((rule, currentIndex) => (currentIndex === index ? nextRule : rule)),
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

  const activeRule = activeRuleIndex === null ? null : (config.rules[activeRuleIndex] ?? null);

  return (
    <div className="space-y-5">
      <div className="rounded-sm border border-line bg-soft px-4 py-3">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div>
            <SectionLabel label="Reminder Rules" />
            <p className="mt-1 max-w-3xl text-sm leading-6 text-fg-soft">
              Reminders inject contextual guidance after matching tool results. Use them to keep
              safety rules, retry guidance, or domain-specific instructions close to the moment they
              matter.
            </p>
          </div>
          <div className="flex flex-wrap gap-2 text-[11px] text-fg-soft">
            <span className="rounded-pill bg-muted px-2 py-0.5">
              {config.rules.length} rule{config.rules.length === 1 ? "" : "s"}
            </span>
            <span className="rounded-pill bg-muted px-2 py-0.5">
              Targets: <span>{summarizeReminderTargets(config)}</span>
            </span>
          </div>
        </div>
      </div>

      <div className="grid gap-5 xl:grid-cols-[18rem,minmax(0,1fr)]">
        <aside className="space-y-3 rounded-sm border border-line bg-soft p-4">
          <div className="flex items-start justify-between gap-3">
            <div>
              <SectionLabel label="Reminder Rules" />
              <p className="mt-2 text-sm leading-6 text-fg-soft">
                Each rule couples a tool-output matcher with the reminder that should be injected.
              </p>
            </div>
            <button
              type="button"
              onClick={addRule}
              className="rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm font-medium text-fg transition hover:border-line-strong hover:bg-muted"
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
                      "w-full rounded-sm border px-4 py-3 text-left transition",
                      isActive
                        ? "border-accent bg-accent text-accent-text shadow-sm"
                        : "border-line bg-surface text-fg-strong hover:border-line-strong",
                    ].join(" ")}
                  >
                    <div className="flex items-center justify-between gap-3">
                      <div className="text-sm font-semibold">
                        {rule.name.trim() || `Reminder ${index + 1}`}
                      </div>
                      <span
                        className={[
                          "rounded-full px-2 py-0.5 text-[11px] font-medium",
                          isActive ? "bg-fg text-bg" : "bg-tone-info/15 text-tone-info",
                        ].join(" ")}
                      >
                        {reminderModeShortLabel(rule.mode)}
                      </span>
                    </div>
                    <div
                      className={["mt-2 text-xs", isActive ? "text-fg-faint" : "text-fg-soft"].join(
                        " ",
                      )}
                    >
                      {rule.tool.trim() ? `Tool: ${rule.tool}` : "Applies to any tool"}
                    </div>
                    <div
                      className={[
                        "mt-2 text-xs font-medium",
                        isActive ? "text-fg-faint" : "text-fg-soft",
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
          <div className="rounded-sm border border-line bg-surface p-5 shadow-sm">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div>
                <h6 className="text-lg font-semibold text-fg-strong">
                  {activeRule.name.trim() || `Reminder ${activeRuleIndex! + 1}`}
                </h6>
                <p className="mt-1 text-sm leading-6 text-fg-soft">
                  Model the trigger first, then define the reminder payload and where it should be
                  injected.
                </p>
              </div>
              <div className="flex flex-wrap gap-2">
                <button
                  type="button"
                  disabled={activeRuleIndex === 0}
                  onClick={() => moveRule(activeRuleIndex!, -1)}
                  className="rounded-sm border border-line-strong px-3 py-2 text-sm font-medium text-fg transition hover:border-line-strong hover:bg-soft disabled:cursor-not-allowed disabled:opacity-50"
                >
                  Move up
                </button>
                <button
                  type="button"
                  disabled={activeRuleIndex === config.rules.length - 1}
                  onClick={() => moveRule(activeRuleIndex!, 1)}
                  className="rounded-sm border border-line-strong px-3 py-2 text-sm font-medium text-fg transition hover:border-line-strong hover:bg-soft disabled:cursor-not-allowed disabled:opacity-50"
                >
                  Move down
                </button>
                <button
                  type="button"
                  onClick={() => removeRule(activeRuleIndex!)}
                  className="rounded-sm border border-tone-error/30 px-3 py-2 text-sm font-medium text-tone-error transition hover:bg-tone-error/10"
                >
                  Remove
                </button>
              </div>
            </div>

            <div className="mt-5 rounded-sm border border-line bg-soft p-4">
              <SectionLabel label="Trigger + Reminder" />
              <p className="mt-2 text-sm leading-6 text-fg-soft">
                Tool pattern and output matcher form the trigger. Reminder content, target, and
                cooldown are the trigger's delivery behavior.
              </p>

              <div className="mt-4 grid gap-4 lg:grid-cols-[minmax(0,1fr),minmax(0,1fr),14rem]">
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
                    className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
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
                    placeholder={TOOL_CALL_PATTERN_PLACEHOLDER}
                    className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 font-mono text-sm text-fg outline-none transition focus:border-fg"
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
                    className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
                  />
                </Field>
              </div>
              <ToolPatternHelp kind="tool-call" />

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

              {activeRule.mode === "content_text" || activeRule.mode === "status_and_text" ? (
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
                      className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
                    />
                  </Field>
                </div>
              ) : null}

              {activeRule.mode === "content_fields" || activeRule.mode === "status_and_fields" ? (
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
                      className="rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm font-medium text-fg transition hover:border-line-strong hover:bg-muted"
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
                          className="grid gap-3 rounded-sm border border-line bg-surface p-3 lg:grid-cols-[minmax(0,1.1fr),12rem,minmax(0,1fr),auto]"
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
                              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
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
                              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
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
                              className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
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
                              className="rounded-sm border border-tone-error/30 px-3 py-2 text-sm font-medium text-tone-error transition hover:bg-tone-error/10"
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

              <div className="mt-5 border-t border-line pt-4">
                <SectionLabel label="Reminder Delivery" />
                <p className="mt-2 text-sm leading-6 text-fg-soft">
                  These fields define what the trigger injects and where the message is placed.
                </p>
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

                <div className="mt-4">
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
                      className="w-full rounded-sm border border-line-strong bg-surface px-3 py-2 text-sm text-fg outline-none transition focus:border-fg"
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
