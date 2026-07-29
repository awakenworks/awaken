import type { CredentialSource } from "../../lib/api/types";
import { SchemaForm, SelectField, type JsonSchema } from "../ui";
import { useApp } from "../../lib/app-state";

export interface WebSearchProviderOption {
  id: string;
  label: string;
  requiresCredential: boolean;
  optionsSchema: JsonSchema;
}

/** Provider choices are projected only from the runtime capability schema. */
export function webSearchProviderOptions(schema?: JsonSchema): WebSearchProviderOption[] {
  const branches = Array.isArray(schema?.oneOf) ? schema.oneOf : [];
  return branches.flatMap((raw) => {
    if (!raw || typeof raw !== "object") return [];
    const branch = raw as JsonSchema;
    const provider = branch.properties?.provider_id;
    const id = typeof provider?.const === "string" ? provider.const : "";
    if (!id) return [];
    return [{
      id,
      label: branch.title ?? (typeof provider?.title === "string" ? provider.title : id),
      requiresCredential: branch.required?.includes("credential") ?? false,
      optionsSchema: branch.properties?.options ?? { type: "object" },
    }];
  });
}

export function defaultWebSearchConfig(schema?: JsonSchema): Record<string, unknown> {
  const provider = webSearchProviderOptions(schema)[0];
  return provider ? { provider_id: provider.id, options: {} } : {};
}

export default function WebSearchBehaviorEditor({
  schema,
  value,
  credentials,
  onChange,
}: {
  schema: JsonSchema;
  value: Record<string, unknown>;
  credentials: CredentialSource[];
  onChange: (next: unknown) => void;
}) {
  const app = useApp();
  const providers = webSearchProviderOptions(schema);
  const selected = providers.find((provider) => provider.id === value.provider_id) ?? providers[0];
  if (!selected) {
    return <span className="err">{app.t("No WebSearch providers are installed.", "未安装 WebSearch 供应商。")}</span>;
  }
  const eligible = credentials.filter((credential) =>
    credential.status === "active"
      && credential.version > 0
      && (!credential.provider_id || credential.provider_id === selected.id));
  const credential = value.credential && typeof value.credential === "object"
    ? value.credential as { id?: string; revision?: number }
    : undefined;

  return (
    <div className="schema-form">
      <SelectField
        label={app.t("Search provider", "搜索供应商")}
        value={selected.id}
        onChange={(event) => onChange({ provider_id: event.target.value, options: {} })}
      >
        {providers.map((provider) => <option key={provider.id} value={provider.id}>{provider.label}</option>)}
      </SelectField>
      {selected.requiresCredential && (
        <SelectField
          label={app.t("Vault credential", "Vault 凭证")}
          value={credential ? `${credential.id}@${credential.revision}` : ""}
          onChange={(event) => {
            const source = eligible.find((candidate) => `${candidate.id}@${candidate.version}` === event.target.value);
            const next = { ...value };
            if (source) next.credential = { id: source.id, revision: source.version };
            else delete next.credential;
            onChange(next);
          }}
        >
          <option value="">{app.t("Select an exact credential revision…", "选择精确凭证版本…")}</option>
          {eligible.map((source) => (
            <option key={`${source.id}@${source.version}`} value={`${source.id}@${source.version}`}>
              {source.id} · r{source.version}
            </option>
          ))}
        </SelectField>
      )}
      <SchemaForm
        schema={selected.optionsSchema}
        value={value.options ?? {}}
        onChange={(options) => onChange({ ...value, provider_id: selected.id, options })}
      />
    </div>
  );
}
