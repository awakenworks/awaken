import Form from "@rjsf/core";
import validator from "@rjsf/validator-ajv8";
import type { ComponentType } from "react";
import type { PluginInfo } from "@/lib/config-api";
import { Pill } from "@/components/form-components";
import {
  pluginConfigDescription,
  pluginConfigDisplayName,
  pluginConfigDisplaySummary,
  pluginConfigEditorKey,
  pluginConfigEntryKey,
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
  hookFilteredOut?: boolean;
  warnings?: string[];
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
      <div className="mt-4 rounded-sm border border-dashed border-line px-4 py-3 text-sm text-fg-soft">
        Select a configurable plugin to edit its agent-level settings.
      </div>
    );
  }

  const activeEntry = activeEntryKey
    ? entries.find(
        (entry) =>
          pluginConfigEntryKey(entry.plugin.id, entry.schema.key) === activeEntryKey,
      )
    : null;
  const activeValue = activeEntry ? sections[activeEntry.schema.key] : undefined;

  return (
    <div className="mt-4 grid gap-5 xl:grid-cols-[18rem,minmax(0,1fr)]">
      <aside className="space-y-3">
        {entries.map((entry) => {
          const key = pluginConfigEntryKey(entry.plugin.id, entry.schema.key);
          const isActive = key === activeEntryKey;
          const summaryValue = sections[entry.schema.key];
          const editorKey = pluginConfigEditorKey(entry.schema);
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
                "w-full rounded-sm border px-4 py-3 text-left transition",
                isActive
                  ? "border-accent bg-accent text-accent-text shadow-sm"
                  : "border-line bg-surface text-fg-strong hover:border-line-strong hover:bg-soft",
              ].join(" ")}
            >
              <div className="flex flex-wrap items-center gap-2">
                <div className="font-medium">
                  {pluginConfigDisplayName(entry.plugin.id, entry.schema)}
                </div>
                <Pill label={statusLabel} active={isActive} />
                {entry.hookFilteredOut ? <Pill label="hook filtered" tone="amber" /> : null}
              </div>
              <div
                className={[
                  "mt-1 text-sm",
                  isActive ? "text-fg-faint" : "text-fg-soft",
                ].join(" ")}
              >
                {schemaTitle(entry.schema.schema) ?? entry.schema.key}
              </div>
              <div
                className={[
                  "mt-2 text-xs",
                  isActive ? "text-fg-faint" : "text-fg-soft",
                ].join(" ")}
              >
                {pluginConfigDisplaySummary(editorKey, summaryValue)}
              </div>
            </button>
          );
        })}
      </aside>

      {activeEntry ? (
        <div className="rounded-sm border border-line bg-surface p-5 shadow-sm">
          <div className="mb-4 border-b border-line pb-4">
            <div className="flex flex-wrap items-center gap-2">
              <h5 className="text-lg font-semibold text-fg-strong">
                {pluginConfigDisplayName(activeEntry.plugin.id, activeEntry.schema)}
              </h5>
              <Pill label={activeEntry.schema.key} />
              {!activeEntry.selected ? (
                <Pill label="plugin disabled" tone="amber" />
              ) : null}
            </div>
            <div className="mt-1 text-sm text-fg-soft">
              {schemaTitle(activeEntry.schema.schema) ?? activeEntry.schema.key}
            </div>
            {pluginConfigDescription(activeEntry.schema) ? (
              <p className="mt-2 text-sm leading-6 text-fg-soft">
                {pluginConfigDescription(activeEntry.schema)}
              </p>
            ) : null}
            {activeEntry.warnings?.length ? (
              <div className="mt-3 space-y-2">
                {activeEntry.warnings.map((warning) => (
                  <div
                    key={warning}
                    className="rounded-sm border border-tone-warn/35 bg-tone-warn/10 px-3 py-2 text-sm text-tone-warn"
                  >
                    {warning}
                  </div>
                ))}
              </div>
            ) : null}
            {!activeEntry.selected ? (
              <div className="mt-3 rounded-sm border border-tone-warn/35 bg-tone-warn/10 px-3 py-2 text-sm text-tone-warn">
                {activeEntry.hasStoredConfig
                  ? `This section is still stored on the agent, but it only takes effect after re-enabling \`${activeEntry.plugin.id}\`.`
                  : `You can preconfigure this section now. It will only take effect after enabling \`${activeEntry.plugin.id}\`.`}
              </div>
            ) : null}
          </div>

          <PluginConfigEditor
            editorKey={pluginConfigEditorKey(activeEntry.schema)}
            schema={activeEntry.schema.schema}
            uiSchema={activeEntry.schema.ui_schema ?? undefined}
            value={activeValue}
            onChange={(value) => onUpdateSection(activeEntry.schema.key, value)}
          />
        </div>
      ) : (
        <div className="rounded-sm border border-dashed border-line bg-surface p-5 text-sm text-fg-soft">
          Select a plugin configuration section to edit it.
        </div>
      )}
    </div>
  );
}

function PluginConfigEditor({
  editorKey,
  schema,
  uiSchema,
  value,
  onChange,
}: {
  editorKey: string;
  schema: Record<string, unknown>;
  uiSchema?: Record<string, unknown> | null;
  value: unknown;
  onChange: (value: unknown) => void;
}) {
  const Editor = SPECIALIZED_PLUGIN_CONFIG_EDITORS[editorKey];
  if (Editor) {
    return <Editor value={value} onChange={onChange} />;
  }

  return (
    <Form
      schema={schema}
      formData={asFormData(value)}
      onChange={({ formData }) => onChange(formData)}
      validator={validator}
      uiSchema={{
        ...(uiSchema ?? {}),
        "ui:submitButtonOptions": { norender: true },
      }}
    >
      <></>
    </Form>
  );
}

const SPECIALIZED_PLUGIN_CONFIG_EDITORS: Record<
  string,
  ComponentType<{ value: unknown; onChange: (value: unknown) => void }>
> = {
  permission: PermissionConfigEditor,
  reminder: ReminderConfigEditor,
  "generative-ui": GenerativeUiConfigEditor,
};

function asFormData(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}
