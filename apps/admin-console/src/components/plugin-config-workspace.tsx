import Form from "@rjsf/core";
import validator from "@rjsf/validator-ajv8";
import { useEffect, useState, type ReactNode } from "react";
import type { PluginInfo } from "@/lib/config-api";
import {
  type GenerativeUiConfig,
  type PermissionBehavior,
  type PermissionConfig,
  type PermissionRuleConfig,
  type ReminderConfigDraft,
  type ReminderFieldConfig,
  type ReminderMode,
  createPermissionRule,
  createReminderRule,
  normalizeGenerativeUiConfig,
  normalizePermissionConfig,
  normalizeReminderConfig,
  pluginDisplayName,
  pluginConfigDisplaySummary,
  pluginConfigEntryKey,
  schemaDescription,
  schemaTitle,
  serializeGenerativeUiConfig,
  serializePermissionConfig,
  serializeReminderConfig,
} from "@/lib/plugin-config";

type ConfigSchema = PluginInfo["config_schemas"][number];

export interface PluginConfigEntry {
  plugin: PluginInfo;
  schema: ConfigSchema;
  selected: boolean;
  hasStoredConfig: boolean;
}

interface PluginConfigWorkspaceProps {
  entries: PluginConfigEntry[];
  sections: Record<string, unknown>;
  activeEntryKey: string | null;
  onSelectEntry: (entryKey: string) => void;
  onUpdateSection: (schemaKey: string, value: unknown) => void;
}

export function PluginConfigWorkspace({
  entries,
  sections,
  activeEntryKey,
  onSelectEntry,
  onUpdateSection,
}: PluginConfigWorkspaceProps) {
  if (entries.length === 0) {
    return (
      <div className="mt-4 rounded-2xl border border-dashed border-slate-200 px-4 py-3 text-sm text-slate-500">
        Select a configurable plugin to edit its agent-level settings.
      </div>
    );
  }

  const activeEntry =
    entries.find(
      (entry) => pluginConfigEntryKey(entry.plugin.id, entry.schema.key) === activeEntryKey,
    ) ?? entries[0];
  const activeValue = sections[activeEntry.schema.key];

  return (
    <div className="mt-4 grid gap-5 xl:grid-cols-[18rem,minmax(0,1fr)]">
      <aside className="space-y-3">
        {entries.map((entry) => {
          const key = pluginConfigEntryKey(entry.plugin.id, entry.schema.key);
          const isActive =
            key === pluginConfigEntryKey(activeEntry.plugin.id, activeEntry.schema.key);
          const summaryValue = sections[entry.schema.key];
          const statusLabel = entry.selected
            ? "enabled"
            : entry.hasStoredConfig
              ? "stored only"
              : "available";
          return (
            <button
              key={key}
              type="button"
              onClick={() => onSelectEntry(key)}
              className={[
                "w-full rounded-2xl border px-4 py-3 text-left transition",
                isActive
                  ? "border-slate-900 bg-slate-900 text-white shadow-sm"
                  : "border-slate-200 bg-white text-slate-900 hover:border-slate-300 hover:bg-slate-50",
              ].join(" ")}
            >
              <div className="flex flex-wrap items-center gap-2">
                <div className="font-medium">{pluginDisplayName(entry.plugin.id)}</div>
                <Pill label={statusLabel} active={isActive} />
              </div>
              <div
                className={[
                  "mt-1 text-sm",
                  isActive ? "text-slate-200" : "text-slate-500",
                ].join(" ")}
              >
                {schemaTitle(entry.schema.schema) ?? entry.schema.key}
              </div>
              <div
                className={[
                  "mt-2 text-xs",
                  isActive ? "text-slate-300" : "text-slate-500",
                ].join(" ")}
              >
                {pluginConfigDisplaySummary(
                  entry.plugin.id,
                  entry.schema.key,
                  summaryValue,
                )}
              </div>
            </button>
          );
        })}
      </aside>

      <div className="rounded-2xl border border-slate-200 bg-white p-5 shadow-sm">
        <div className="mb-4 border-b border-slate-200 pb-4">
          <div className="flex flex-wrap items-center gap-2">
            <h5 className="text-lg font-semibold text-slate-950">
              {pluginDisplayName(activeEntry.plugin.id)}
            </h5>
            <Pill label={activeEntry.schema.key} />
            {!activeEntry.selected ? <Pill label="plugin disabled" tone="amber" /> : null}
          </div>
          <div className="mt-1 text-sm text-slate-500">
            {schemaTitle(activeEntry.schema.schema) ?? activeEntry.schema.key}
          </div>
          {schemaDescription(activeEntry.schema.schema) ? (
            <p className="mt-2 text-sm leading-6 text-slate-600">
              {schemaDescription(activeEntry.schema.schema)}
            </p>
          ) : null}
          {!activeEntry.selected ? (
            <div className="mt-3 rounded-xl border border-amber-200 bg-amber-50 px-3 py-2 text-sm text-amber-700">
              {activeEntry.hasStoredConfig
                ? `This section is still stored on the agent, but it only takes effect after re-enabling \`${activeEntry.plugin.id}\`.`
                : `You can preconfigure this section now. It will only take effect after enabling \`${activeEntry.plugin.id}\`.`}
            </div>
          ) : null}
        </div>

        <PluginConfigEditor
          pluginId={activeEntry.plugin.id}
          schemaKey={activeEntry.schema.key}
          schema={activeEntry.schema.schema}
          value={activeValue}
          onChange={(value) => onUpdateSection(activeEntry.schema.key, value)}
        />
      </div>
    </div>
  );
}

function PluginConfigEditor({
  pluginId,
  schemaKey,
  schema,
  value,
  onChange,
}: {
  pluginId: string;
  schemaKey: string;
  schema: Record<string, unknown>;
  value: unknown;
  onChange: (value: unknown) => void;
}) {
  if (pluginId === "permission" || schemaKey === "permission") {
    return <PermissionConfigEditor value={value} onChange={onChange} />;
  }

  if (pluginId === "reminder" || schemaKey === "reminder") {
    return <ReminderConfigEditor value={value} onChange={onChange} />;
  }

  if (pluginId === "generative-ui" || schemaKey === "generative-ui") {
    return <GenerativeUiConfigEditor value={value} onChange={onChange} />;
  }

  return (
    <Form
      schema={schema}
      formData={asFormData(value)}
      onChange={({ formData }) => onChange(formData)}
      validator={validator}
      uiSchema={{ "ui:submitButtonOptions": { norender: true } }}
    >
      <></>
    </Form>
  );
}

function PermissionConfigEditor({
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

function ReminderConfigEditor({
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

function GenerativeUiConfigEditor({
  value,
  onChange,
}: {
  value: unknown;
  onChange: (value: unknown) => void;
}) {
  const config = normalizeGenerativeUiConfig(value);

  function update(nextConfig: GenerativeUiConfig) {
    onChange(serializeGenerativeUiConfig(nextConfig));
  }

  return (
    <div className="space-y-4">
      <Field label="Catalog ID">
        <input
          type="text"
          value={config.catalog_id}
          onChange={(event) =>
            update({ ...config, catalog_id: event.target.value })
          }
          placeholder="https://a2ui.org/specification/v0_8/standard_catalog_definition.json"
          className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
        />
      </Field>
      <Field label="Examples">
        <textarea
          value={config.examples}
          onChange={(event) => update({ ...config, examples: event.target.value })}
          rows={6}
          className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
        />
      </Field>
      <Field label="Instruction override">
        <textarea
          value={config.instructions}
          onChange={(event) =>
            update({ ...config, instructions: event.target.value })
          }
          rows={8}
          className="w-full rounded-xl border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none transition focus:border-slate-500"
        />
      </Field>
      <Hint>
        Leave fields empty to keep the plugin defaults. Setting an instruction
        override takes precedence over catalog and examples.
      </Hint>
    </div>
  );
}

function Field({
  label,
  children,
}: {
  label: string;
  children: ReactNode;
}) {
  return (
    <label className="block">
      <span className="mb-1.5 block text-sm font-medium text-slate-600">{label}</span>
      {children}
    </label>
  );
}

function Hint({ children }: { children: ReactNode }) {
  return <div className="text-sm leading-6 text-slate-500">{children}</div>;
}

function SectionLabel({ label }: { label: string }) {
  return (
    <div className="text-xs font-semibold uppercase tracking-[0.18em] text-slate-500">
      {label}
    </div>
  );
}

function EmptyState({
  message,
  compact = false,
}: {
  message: string;
  compact?: boolean;
}) {
  return (
    <div
      className={[
        "rounded-2xl border border-dashed border-slate-200 text-sm text-slate-500",
        compact ? "px-4 py-3" : "mt-4 px-4 py-5",
      ].join(" ")}
    >
      {message}
    </div>
  );
}

function MetricCard({
  label,
  value,
  detail,
}: {
  label: string;
  value: string;
  detail: string;
}) {
  return (
    <div className="rounded-2xl border border-slate-200 bg-white p-4">
      <div className="text-xs font-semibold uppercase tracking-[0.18em] text-slate-500">
        {label}
      </div>
      <div className="mt-2 text-lg font-semibold text-slate-950">{value}</div>
      <div className="mt-1 text-sm leading-6 text-slate-500">{detail}</div>
    </div>
  );
}

function Pill({
  label,
  active = false,
  tone = "slate",
}: {
  label: string;
  active?: boolean;
  tone?: "slate" | "amber";
}) {
  const palette =
    tone === "amber"
      ? active
        ? "bg-amber-200 text-amber-950"
        : "bg-amber-100 text-amber-700"
      : active
        ? "bg-slate-700 text-slate-100"
        : "bg-slate-200 text-slate-600";
  return (
    <span className={`rounded-full px-2 py-0.5 text-xs font-medium ${palette}`}>
      {label}
    </span>
  );
}

function ChoiceGrid<T extends string>({
  value,
  options,
  onChange,
  columns,
}: {
  value: T;
  options: Array<{
    value: T;
    label: string;
    description: string;
  }>;
  onChange: (value: T) => void;
  columns: string;
}) {
  return (
    <div className={["grid gap-2", columns].join(" ")}>
      {options.map((option) => {
        const selected = option.value === value;
        return (
          <button
            key={option.value}
            type="button"
            onClick={() => onChange(option.value)}
            className={[
              "rounded-2xl border px-3 py-3 text-left transition",
              selected
                ? "border-slate-900 bg-slate-900 text-white shadow-sm"
                : "border-slate-200 bg-white text-slate-900 hover:border-slate-300 hover:bg-slate-50",
            ].join(" ")}
          >
            <div className="text-sm font-semibold">{option.label}</div>
            <div
              className={[
                "mt-1 text-xs leading-5",
                selected ? "text-slate-300" : "text-slate-500",
              ].join(" ")}
            >
              {option.description}
            </div>
          </button>
        );
      })}
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

function createReminderField(): ReminderFieldConfig {
  return {
    path: "",
    op: "glob",
    value: "",
  };
}

function asFormData(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function moveItem<T>(items: T[], fromIndex: number, toIndex: number): T[] {
  const next = [...items];
  const [item] = next.splice(fromIndex, 1);
  next.splice(toIndex, 0, item);
  return next;
}
