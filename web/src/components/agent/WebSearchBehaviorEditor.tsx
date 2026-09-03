import type { CredentialSource } from "../../lib/api/types";
import { SchemaForm, SelectField, type JsonSchema } from "../ui";
import { useApp } from "../../lib/app-state";

export interface WebSearchProviderOption {
  id: string;
  label: string;
  requiresCredential: boolean;
  optionsSchema: JsonSchema;
  realization: "host_executed" | "provider_server";
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
      realization: branch["x-awaken-realization"] === "provider_server" ? "provider_server" : "host_executed",
    }];
  });
}

export function defaultWebSearchConfig(schema?: JsonSchema): Record<string, unknown> {
  const provider = webSearchProviderOptions(schema)[0];
  return provider ? { provider_id: provider.id, options: {} } : {};
}

export default function WebSearchBehaviorEditor({
  toolId = "web_search",
  schema,
  value,
  credentials,
  onChange,
}: {
  toolId?: "web_search" | "web_fetch";
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
  const eligibleFor = (providerId: string) => credentials.filter((credential) =>
    credential.status === "active"
      && credential.version > 0
      && (!credential.provider_id || credential.provider_id === providerId));
  const eligible = eligibleFor(selected.id);
  const credential = value.credential && typeof value.credential === "object"
    ? value.credential as { id?: string; revision?: number }
    : undefined;
  const fallbacks = Array.isArray(value.fallbacks)
    ? value.fallbacks.filter((target): target is Record<string, unknown> => !!target && typeof target === "object")
    : [];
  const updateFallback = (index: number, next: Record<string, unknown> | null) => {
    const updated = [...fallbacks];
    if (next) updated[index] = next;
    else updated.splice(index, 1);
    onChange({ ...value, fallbacks: updated });
  };
  const providerLabel = toolId === "web_search"
    ? app.t("Search provider / account", "搜索供应商 / 账户")
    : app.t("Fetch provider / account", "抓取供应商 / 账户");

  return (
    <div className="schema-form">
      <SelectField
        label={providerLabel}
        value={selected.id}
        onChange={(event) => onChange({ provider_id: event.target.value, options: {}, fallbacks: [] })}
      >
        {providers.map((provider) => <option key={provider.id} value={provider.id}>{provider.label}</option>)}
      </SelectField>
      <div className="mut" style={{ fontSize: 12 }}>
        {selected.realization === "provider_server"
          ? app.t("Executed by the exact model provider. It uses that model route/account; no separate web credential or fallback is stored here. Per-call Awaken approval is unavailable for provider-executed tools.", "由精确的模型供应商执行；复用该模型路由/账户，此处不保存独立 Web 凭证或 fallback。供应商执行的工具不支持 Awaken 逐次审批。")
          : app.t("Executed by Awaken. Provider accounts are exact credential pins and fail over only before dispatch.", "由 Awaken 执行；供应商账户由精确凭证版本表示，并且仅在请求发出前失败时切换。")}
      </div>
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
      {selected.realization === "host_executed" && (
        <div className="schema-form">
          <div className="behavior-title">{app.t("Fallback accounts", "备用账户")}</div>
          {fallbacks.map((fallback, index) => {
            const fallbackProvider = providers.find((provider) => provider.id === fallback.provider_id && provider.realization === "host_executed")
              ?? providers.find((provider) => provider.realization === "host_executed");
            if (!fallbackProvider) return null;
            const fallbackCredential = fallback.credential && typeof fallback.credential === "object"
              ? fallback.credential as { id?: string; revision?: number }
              : undefined;
            const fallbackEligible = eligibleFor(fallbackProvider.id);
            return (
              <div key={index} className="card" style={{ padding: 10 }}>
                <SelectField
                  label={app.t(`Fallback ${index + 1}`, `备用 ${index + 1}`)}
                  value={fallbackProvider.id}
                  onChange={(event) => updateFallback(index, { provider_id: event.target.value, options: {} })}
                >
                  {providers.filter((provider) => provider.realization === "host_executed").map((provider) => (
                    <option key={provider.id} value={provider.id}>{provider.label}</option>
                  ))}
                </SelectField>
                {fallbackProvider.requiresCredential && (
                  <SelectField
                    label={app.t("Exact account credential", "精确账户凭证")}
                    value={fallbackCredential ? `${fallbackCredential.id}@${fallbackCredential.revision}` : ""}
                    onChange={(event) => {
                      const source = fallbackEligible.find((candidate) => `${candidate.id}@${candidate.version}` === event.target.value);
                      const next: Record<string, unknown> = { ...fallback, provider_id: fallbackProvider.id };
                      if (source) next.credential = { id: source.id, revision: source.version };
                      else delete next.credential;
                      updateFallback(index, next);
                    }}
                  >
                    <option value="">{app.t("Select…", "选择…")}</option>
                    {fallbackEligible.map((source) => <option key={`${source.id}@${source.version}`} value={`${source.id}@${source.version}`}>{source.id} · r{source.version}</option>)}
                  </SelectField>
                )}
                <SchemaForm schema={fallbackProvider.optionsSchema} value={fallback.options ?? {}} onChange={(options) => updateFallback(index, { ...fallback, provider_id: fallbackProvider.id, options })} />
                <button type="button" className="btn" onClick={() => updateFallback(index, null)}>{app.t("Remove fallback", "移除备用")}</button>
              </div>
            );
          })}
          <button
            type="button"
            className="btn"
            onClick={() => {
              const provider = providers.find((candidate) => candidate.realization === "host_executed");
              if (provider) onChange({ ...value, fallbacks: [...fallbacks, { provider_id: provider.id, options: {} }] });
            }}
          >
            {app.t("Add fallback account", "添加备用账户")}
          </button>
        </div>
      )}
    </div>
  );
}
