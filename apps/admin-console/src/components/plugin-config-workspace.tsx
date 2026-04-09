import Form from "@rjsf/core";
import validator from "@rjsf/validator-ajv8";
import type { PluginInfo } from "@/lib/config-api";
import { Pill } from "@/components/form-components";
import {
  pluginDisplayName,
  pluginConfigDisplaySummary,
  pluginConfigEntryKey,
  schemaDescription,
  schemaTitle,
} from "@/lib/plugin-config";
import { PermissionConfigEditor } from "@/components/editors/permission-editor";
import { ReminderConfigEditor } from "@/components/editors/reminder-editor";
import { GenerativeUiConfigEditor } from "@/components/editors/generative-ui-editor";

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

function asFormData(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}
