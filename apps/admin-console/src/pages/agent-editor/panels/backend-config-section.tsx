import Form from "@rjsf/core";
import validator from "@rjsf/validator-ajv8";
import { type AgentSpec, type Capabilities } from "@/lib/config-api";
import { redactSecretsForDisplay } from "@/lib/agent-editor-helpers";

export function BackendConfigSection({
  backend,
  capabilities,
  onChange,
}: {
  backend: NonNullable<AgentSpec["backend"]>;
  capabilities: Capabilities | null;
  onChange?: (config: Record<string, unknown>) => void;
}) {
  const backendInfo = capabilities?.backends?.find((candidate) => candidate.kind === backend.kind);
  const title =
    backendInfo?.display_name ??
    (backend.kind === "awaken" ? "Awaken" : backend.kind.toUpperCase());
  const schemaTitle =
    typeof backendInfo?.schema?.title === "string" ? backendInfo.schema.title : null;
  const editable = Boolean(onChange);
  const config = backend.config ?? {};
  const formConfig = editable ? config : redactSecretsForDisplay(config);
  const previewConfig = redactSecretsForDisplay(config);

  return (
    <section className="rounded-sm border border-line bg-surface p-5 shadow-sm">
      <h3 className="text-lg font-semibold text-fg-strong">Backend</h3>
      <p className="mt-2 max-w-xl text-sm text-fg-soft">
        This agent uses the <span className="font-mono">{backend.kind}</span> backend
        {backend.version ? ` config v${backend.version}` : ""}. Backend configuration is schema
        driven.
      </p>
      {backendInfo?.description ? (
        <p className="mt-2 max-w-xl text-sm text-fg-soft">{backendInfo.description}</p>
      ) : null}
      <div className="mt-3 flex flex-wrap gap-2 text-xs text-fg-soft">
        <span className="rounded-full border border-line px-2 py-1">{title}</span>
        {schemaTitle ? (
          <span className="rounded-full border border-line px-2 py-1">{schemaTitle}</span>
        ) : null}
      </div>
      {backendInfo?.schema ? (
        <div className="mt-4" data-testid="backend-config-schema-form">
          <Form
            schema={backendInfo.schema}
            formData={asBackendFormData(formConfig)}
            disabled={!editable}
            readonly={!editable}
            onChange={({ formData }) => onChange?.(asBackendFormData(formData))}
            validator={validator}
            uiSchema={{
              ...(backendInfo.ui_schema ?? {}),
              "ui:submitButtonOptions": { norender: true },
            }}
          >
            <></>
          </Form>
        </div>
      ) : (
        <pre
          className="mt-3 overflow-auto rounded-sm bg-code-bg p-3 text-xs text-code-fg"
          data-testid="backend-config-readonly"
        >
          {JSON.stringify(previewConfig, null, 2)}
        </pre>
      )}
    </section>
  );
}

function asBackendFormData(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}
