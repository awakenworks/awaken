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
        <div className="rounded-2xl border border-slate-200 bg-slate-50 p-4">
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

        <div className="rounded-2xl border border-slate-200 bg-white p-4">
          <SectionLabel label="Default Decision" />
          <p className="mt-2 text-sm leading-6 text-slate-500">
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
        <aside className="space-y-3 rounded-2xl border border-slate-200 bg-slate-50 p-4">
          <div className="flex items-start justify-between gap-3">
            <div>
              <SectionLabel label="Permission Rules" />
              <p className="mt-2 text-sm leading-6 text-slate-500">
                Match tool names or patterns like `Bash(npm *)` or
                `mcp__github__*`.
              </p>
            </div>
            <button
              type="button"
              onClick={addRule}
              className="rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm font-medium text-slate-700 transition hover:border-slate-400 hover:bg-slate-100"
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
                      "w-full rounded-2xl border px-4 py-3 text-left transition",
                      isActive
                        ? "border-slate-900 bg-slate-900 text-white shadow-sm"
                        : "border-slate-200 bg-white text-slate-900 hover:border-slate-300",
                    ].join(" ")}
                  >
                    <div className="flex items-center justify-between gap-3">
                      <div className="text-sm font-semibold">
                        {rule.tool.trim() || `Rule ${index + 1}`}
                      </div>
                      <span
                        className={[
                          "rounded-full px-2 py-0.5 text-[11px] font-medium",
                          isActive
                            ? "bg-slate-700 text-slate-100"
                            : permissionBehaviorPill(rule.behavior),
                        ].join(" ")}
                      >
                        {permissionBehaviorLabel(rule.behavior)}
                      </span>
                    </div>
                    <div
                      className={[
                        "mt-2 text-xs",
                        isActive ? "text-slate-300" : "text-slate-500",
                      ].join(" ")}
                    >
                      {rule.tool.trim()
                        ? `Pattern: ${rule.tool}`
                        : "Matches any tool that reaches this rule."}
                    </div>
                    <div
                      className={[
                        "mt-2 text-xs font-medium",
                        isActive ? "text-slate-300" : "text-slate-500",
                      ].join(" ")}
                    >
                      Scope: {permissionScopeLabel(rule.scope)}
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
                  {activeRule.tool.trim() || `Rule ${activeRuleIndex! + 1}`}
                </h6>
                <p className="mt-1 text-sm leading-6 text-slate-500">
                  Reorder rules to make precedence explicit. The first matching
                  rule decides what happens.
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
                  className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
                />
              </Field>
              <div className="rounded-2xl border border-slate-200 bg-slate-50 p-4">
                <SectionLabel label="Match Preview" />
                <div className="mt-3 text-sm text-slate-700">
                  {activeRule.tool.trim()
                    ? activeRule.tool
                    : "Any tool that reaches this rule"}
                </div>
                <div className="mt-2 text-xs leading-6 text-slate-500">
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
      return "bg-emerald-100 text-emerald-700";
    case "ask":
      return "bg-amber-100 text-amber-700";
    case "deny":
      return "bg-rose-100 text-rose-700";
  }
}
