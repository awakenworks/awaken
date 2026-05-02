import { useEffect, useState } from "react";
import {
  type PermissionBehavior,
  type PermissionConfig,
  type PermissionRuleConfig,
  createPermissionRule,
  moveItem,
  normalizePermissionConfig,
  serializePermissionConfig,
} from "@/lib/plugin-config";
import {
  ChoiceGrid,
  EmptyState,
  Field,
  Hint,
  MetricCard,
  SectionLabel,
} from "@/components/form-components";

export function PermissionConfigEditor({
  value,
  onChange,
}: {
  value: unknown;
  onChange: (value: unknown) => void;
}) {
  const config = normalizePermissionConfig(value);
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

  function update(nextConfig: PermissionConfig) {
    onChange(serializePermissionConfig(nextConfig));
  }

  function updateRule(index: number, nextRule: PermissionRuleConfig) {
    update({
      ...config,
      rules: config.rules.map((rule, currentIndex) =>
        currentIndex === index ? nextRule : rule,
      ),
    });
  }

  function addRule() {
    const nextRules = [...config.rules, createPermissionRule()];
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
        <div className="rounded-2xl border border-line bg-soft p-4">
          <SectionLabel label="Policy Summary" />
          <div className="mt-3 grid gap-3 sm:grid-cols-2 xl:grid-cols-1">
            <MetricCard
              label="Default decision"
              value={permissionBehaviorLabel(config.default_behavior)}
              detail="Used when no rule matches."
            />
            <MetricCard
              label="Ordered rules"
              value={`${config.rules.length}`}
              detail="First matching rule wins."
            />
          </div>
        </div>

        <div className="rounded-2xl border border-line bg-surface p-4">
          <SectionLabel label="Default Decision" />
          <p className="mt-2 text-sm leading-6 text-fg-soft">
            This becomes the fallback policy when none of the explicit rules
            below match the tool call.
          </p>
          <div className="mt-4">
            <ChoiceGrid
              value={config.default_behavior}
              onChange={(nextValue) =>
                update({
                  ...config,
                  default_behavior: nextValue,
                })
              }
              columns="md:grid-cols-3"
              options={[
                {
                  value: "allow",
                  label: "Allow",
                  description: "Run immediately without asking.",
                },
                {
                  value: "ask",
                  label: "Ask",
                  description: "Pause and request confirmation.",
                },
                {
                  value: "deny",
                  label: "Deny",
                  description: "Block the tool call.",
                },
              ]}
            />
          </div>
          <Hint>
            Runtime evaluation is top-to-bottom. Keep broad catch-all rules lower
            than specific exceptions.
          </Hint>
        </div>
      </div>

      <div className="grid gap-5 xl:grid-cols-[18rem,minmax(0,1fr)]">
        <aside className="space-y-3 rounded-2xl border border-line bg-soft p-4">
          <div className="flex items-start justify-between gap-3">
            <div>
              <SectionLabel label="Permission Rules" />
              <p className="mt-2 text-sm leading-6 text-fg-soft">
                Match tool names or patterns like `Bash(npm *)` or
                `mcp__github__*`.
              </p>
            </div>
            <button
              type="button"
              onClick={addRule}
              className="rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm font-medium text-fg transition hover:border-line-strong hover:bg-muted"
            >
              Add rule
            </button>
          </div>

          {config.rules.length === 0 ? (
            <EmptyState message="No explicit permission rules configured." compact />
          ) : (
            <div className="space-y-2">
              {config.rules.map((rule, index) => {
                const isActive = index === activeRuleIndex;
                return (
                  <button
                    key={`permission-rule-${index}`}
                    type="button"
                    onClick={() => setActiveRuleIndex(index)}
                    className={[
                      "group flex w-full items-stretch gap-2 rounded-md border px-3 py-2.5 text-left transition-colors",
                      isActive
                        ? "border-fg-strong bg-fg-strong text-bg shadow-card"
                        : "border-line bg-surface text-fg-strong hover:border-line-strong",
                    ].join(" ")}
                  >
                    <span
                      aria-hidden
                      title="Reorder via Move up / Move down on the right pane"
                      className={[
                        "mt-0.5 select-none font-mono text-xs leading-none",
                        isActive ? "text-fg-faint" : "text-fg-faint group-hover:text-fg-soft",
                      ].join(" ")}
                    >
                      ⋮⋮
                    </span>
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center justify-between gap-2">
                        <div className="flex items-baseline gap-2 min-w-0">
                          <span
                            className={[
                              "font-mono text-[10px]",
                              isActive ? "text-fg-faint" : "text-fg-faint",
                            ].join(" ")}
                          >
                            #{String(index + 1).padStart(2, "0")}
                          </span>
                          <span className="truncate font-mono text-sm">
                            {rule.tool.trim() || "(any tool)"}
                          </span>
                        </div>
                        <span
                          className={[
                            "shrink-0 rounded-pill px-2 py-0.5 text-[10px] font-medium uppercase tracking-eyebrow",
                            isActive
                              ? "bg-fg text-bg"
                              : permissionBehaviorPill(rule.behavior),
                          ].join(" ")}
                        >
                          {permissionBehaviorLabel(rule.behavior)}
                        </span>
                      </div>
                      <div
                        className={[
                          "mt-1 text-[11px]",
                          isActive ? "text-fg-faint" : "text-fg-soft",
                        ].join(" ")}
                      >
                        Scope · {permissionScopeLabel(rule.scope)}
                      </div>
                    </div>
                  </button>
                );
              })}
            </div>
          )}
        </aside>

        {activeRule ? (
          <div className="rounded-2xl border border-line bg-surface p-5 shadow-sm">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div>
                <h6 className="text-lg font-semibold text-fg-strong">
                  {activeRule.tool.trim() || `Rule ${activeRuleIndex! + 1}`}
                </h6>
                <p className="mt-1 text-sm leading-6 text-fg-soft">
                  Reorder rules to make precedence explicit. The first matching
                  rule decides what happens.
                </p>
              </div>
              <div className="flex flex-wrap gap-2">
                <button
                  type="button"
                  disabled={activeRuleIndex === 0}
                  onClick={() => moveRule(activeRuleIndex!, -1)}
                  className="rounded-xl border border-line-strong px-3 py-2 text-sm font-medium text-fg transition hover:border-line-strong hover:bg-soft disabled:cursor-not-allowed disabled:opacity-50"
                >
                  Move up
                </button>
                <button
                  type="button"
                  disabled={activeRuleIndex === config.rules.length - 1}
                  onClick={() => moveRule(activeRuleIndex!, 1)}
                  className="rounded-xl border border-line-strong px-3 py-2 text-sm font-medium text-fg transition hover:border-line-strong hover:bg-soft disabled:cursor-not-allowed disabled:opacity-50"
                >
                  Move down
                </button>
                <button
                  type="button"
                  onClick={() => removeRule(activeRuleIndex!)}
                  className="rounded-xl border border-tone-error/30 px-3 py-2 text-sm font-medium text-tone-error transition hover:bg-tone-error/10"
                >
                  Remove
                </button>
              </div>
            </div>

            <div className="mt-5 grid gap-4 lg:grid-cols-[minmax(0,1.5fr),18rem]">
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
                  placeholder="Bash(npm *)"
                  className="w-full rounded-xl border border-line-strong bg-surface px-3 py-2 text-sm text-fg-strong outline-none transition focus:border-line-strong"
                />
              </Field>
              <div className="rounded-2xl border border-line bg-soft p-4">
                <SectionLabel label="Match Preview" />
                <div className="mt-3 text-sm text-fg">
                  {activeRule.tool.trim()
                    ? activeRule.tool
                    : "Any tool that reaches this rule"}
                </div>
                <div className="mt-2 text-xs leading-6 text-fg-soft">
                  Common examples: `Bash(npm *)`, `Edit(src/**)`,
                  `mcp__github__*`.
                </div>
              </div>
            </div>

            <div className="mt-5 grid gap-5 lg:grid-cols-2">
              <div>
                <SectionLabel label="Decision" />
                <div className="mt-3">
                  <ChoiceGrid
                    value={activeRule.behavior}
                    onChange={(nextValue) =>
                      updateRule(activeRuleIndex!, {
                        ...activeRule,
                        behavior: nextValue,
                      })
                    }
                    columns="md:grid-cols-3"
                    options={[
                      {
                        value: "allow",
                        label: "Allow",
                        description: "Run immediately.",
                      },
                      {
                        value: "ask",
                        label: "Ask",
                        description: "Pause for confirmation.",
                      },
                      {
                        value: "deny",
                        label: "Deny",
                        description: "Reject the call.",
                      },
                    ]}
                  />
                </div>
              </div>

              <div>
                <SectionLabel label="Remember For" />
                <div className="mt-3">
                  <ChoiceGrid
                    value={activeRule.scope}
                    onChange={(nextValue) =>
                      updateRule(activeRuleIndex!, {
                        ...activeRule,
                        scope: nextValue,
                      })
                    }
                    columns="md:grid-cols-2"
                    options={[
                      {
                        value: "project",
                        label: "Project",
                        description: "Persist across the project.",
                      },
                      {
                        value: "thread",
                        label: "Thread",
                        description: "Keep it within this thread.",
                      },
                      {
                        value: "session",
                        label: "Session",
                        description: "Until the current session ends.",
                      },
                      {
                        value: "once",
                        label: "Once",
                        description: "Apply only to the next call.",
                      },
                      {
                        value: "user",
                        label: "User",
                        description: "Persist for the user profile.",
                      },
                    ]}
                  />
                </div>
              </div>
            </div>
          </div>
        ) : (
          <EmptyState message="Add a rule to define explicit permission behavior for matching tools." />
        )}
      </div>
    </div>
  );
}

function permissionBehaviorLabel(behavior: PermissionBehavior): string {
  switch (behavior) {
    case "allow":
      return "Allow";
    case "ask":
      return "Ask";
    case "deny":
      return "Deny";
  }
}

function permissionScopeLabel(scope: PermissionRuleConfig["scope"]): string {
  switch (scope) {
    case "project":
      return "Project";
    case "thread":
      return "Thread";
    case "session":
      return "Session";
    case "once":
      return "Once";
    case "user":
      return "User";
  }
}

function permissionBehaviorPill(behavior: PermissionBehavior): string {
  switch (behavior) {
    case "allow":
      return "bg-tone-success/15 text-tone-success";
    case "ask":
      return "bg-tone-warn/15 text-tone-warn";
    case "deny":
      return "bg-tone-error/15 text-tone-error";
  }
}
