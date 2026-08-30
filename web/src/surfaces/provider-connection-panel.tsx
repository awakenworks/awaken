import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import {
  Button,
  Card,
  SecretField,
  SelectField,
  TextField,
} from "../components/ui";
import { api, workspaceFields, ws } from "../lib/api/client";
import type {
  CredentialSource,
  ProviderCatalog,
  ProviderConnectionView,
  ProviderConnectionSummary,
  ProviderDriverDescriptor,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";

interface ProviderConnectionPanelProps {
  credentials: CredentialSource[];
  descriptors: ProviderDriverDescriptor[];
  connections: ProviderConnectionSummary[];
  catalog?: ProviderCatalog;
}

interface ProviderDraft {
  provider: string;
  endpointName: string;
  baseUrl: string;
  dialect: string;
}

function connectionStatusLabel(
  status: ProviderConnectionSummary["status"] | undefined,
  t: (en: string, zh: string) => string,
) {
  switch (status) {
    case "ready": return t("Ready", "可用");
    case "connected": return t("Connected", "已连接");
    case "stale": return t("Needs recheck", "需要重新检查");
    case "needs_attention": return t("Needs attention", "需要处理");
    case "unavailable": return t("Unavailable", "不可用");
    default: return t("Not connected", "未连接");
  }
}

function dialectLabel(dialect: string, t: (en: string, zh: string) => string) {
  switch (dialect) {
    case "anthropic_messages": return t("Anthropic Messages", "Anthropic Messages 格式");
    case "open_ai_responses": return t("OpenAI Responses", "OpenAI Responses 格式");
    case "open_ai_chat": return t("OpenAI Chat Completions", "OpenAI Chat Completions 格式");
    case "gemini": return t("Gemini API", "Gemini API 格式");
    case "vertex_gemini": return t("Vertex AI Gemini", "Vertex AI Gemini 格式");
    default: return dialect;
  }
}

function credentialLabel(credential: CredentialSource) {
  const parts = credential.id.split(":");
  const modelMarker = parts.lastIndexOf("model");
  const storedLabel = modelMarker >= 0 ? parts[modelMarker + 2] : undefined;
  return storedLabel?.replaceAll("-", " ") || credential.env_key || credential.provider_id || credential.id;
}

export function providerModelCountLabel(count: number, locale: "en" | "zh"): string {
  if (locale === "zh") return `${count} 个模型`;
  return `${count} ${count === 1 ? "model" : "models"}`;
}

export function providerDraftDefaults(descriptor: ProviderDriverDescriptor) {
  const endpoint = descriptor.default_endpoints[0];
  return {
    provider: descriptor.provider_kind,
    endpointName: "",
    baseUrl: endpoint?.base_url ?? "",
    dialect: endpoint?.dialect ?? descriptor.supported_dialects[0],
  };
}

export function providerConfigurationDefaults(descriptor: ProviderDriverDescriptor) {
  return Object.fromEntries(
    descriptor.configuration_fields
      .filter((field) => field.kind !== "secret")
      .map((field) => [field.key, field.placeholder ?? ""]),
  );
}

export function credentialReusableForProvider(
  credential: CredentialSource,
  descriptor: ProviderDriverDescriptor | undefined,
): boolean {
  if (!descriptor || credential.status !== "active") return false;
  if (credential.env_key === "CLAUDE_CODE_OAUTH_TOKEN") return false;
  // Untagged Vault entries are Runtime Secrets (environment variables, static
  // bearer tokens, or MCP OAuth material), not model API keys.  Reuse only a
  // credential that was explicitly classified for this provider.
  if (credential.provider_id !== descriptor.provider_kind) return false;
  if (credential.kind === "oauth") return descriptor.auth_methods.includes("oauth");
  return credential.kind === "vault" && descriptor.auth_methods.includes("api_key");
}

export default function ProviderConnectionPanel({
  credentials,
  descriptors,
  connections,
  catalog,
}: ProviderConnectionPanelProps) {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const [draft, setDraft] = useState<ProviderDraft>({
    provider: "anthropic",
    endpointName: "",
    baseUrl: "",
    dialect: "anthropic_messages",
  });
  const [apiKey, setApiKey] = useState("");
  const [credentialName, setCredentialName] = useState("Anthropic · primary");
  const [authMode, setAuthMode] = useState<"api_key" | "oauth" | "existing">(
    "api_key",
  );
  const [configuration, setConfiguration] = useState<Record<string, string>>({});
  const [syncCredential, setSyncCredential] = useState("");
  const [idempotencyKey, setIdempotencyKey] = useState(() => crypto.randomUUID());
  const initialized = useRef(false);
  const selectedDescriptor = descriptors.find(
    (descriptor) => descriptor.provider_kind === draft.provider,
  );

  const selectDescriptor = (descriptor: ProviderDriverDescriptor) => {
    const existing = credentials.find(
      (credential) =>
        credential.status === "active" &&
        credential.provider_id === descriptor.provider_kind,
    );
    const savedConnection = connections.find((connection) => connection.provider_id === descriptor.provider_kind);
    const savedEndpoint = savedConnection?.endpoint_ids
      .map((id) => catalog?.endpoints[id])
      .find(Boolean);
    const defaults = providerDraftDefaults(descriptor);
    setDraft({
      ...draft,
      ...defaults,
      ...(savedEndpoint ? { baseUrl: savedEndpoint.base_url ?? defaults.baseUrl, dialect: savedEndpoint.dialect } : {}),
    });
    setConfiguration(providerConfigurationDefaults(descriptor));
    setApiKey("");
    setCredentialName(`${descriptor.display_name} · primary`);
    setAuthMode(existing ? "existing" : descriptor.auth_methods[0] ?? "api_key");
    setSyncCredential(existing?.id ?? "");
  };

  useEffect(() => {
    if (initialized.current || descriptors.length === 0) return;
    initialized.current = true;
    selectDescriptor(
      descriptors.find((descriptor) => descriptor.provider_kind === draft.provider)
        ?? descriptors[0],
    );
  // Initialization follows the first authoritative descriptor snapshot only.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [descriptors]);

  const connect = useMutation({
    mutationFn: () => {
      return api.post<ProviderConnectionView>(
        ws("/v1/config/provider-connections"),
        {
          idempotency_key: idempotencyKey,
          ...workspaceFields(workspace),
          provider_id: draft.provider,
          display_name: selectedDescriptor?.display_name ?? draft.provider,
          credential_name: credentialName.trim() || `${draft.provider} primary`,
          dialect: draft.dialect,
          ...(draft.endpointName ? { endpoint_name: draft.endpointName } : {}),
          base_url: draft.baseUrl || null,
          configuration,
          timeout_secs: 60,
          ...(authMode === "api_key" ? { secret: apiKey } : {}),
          ...(authMode === "oauth" ? { oauth_helper: "gcloud" } : {}),
          ...(authMode === "existing" && syncCredential
            ? { credential_source_id: syncCredential }
            : {}),
        },
      );
    },
    onSuccess: (connection) => {
      setIdempotencyKey(crypto.randomUUID());
      setApiKey("");
      setSyncCredential(connection.credential.id);
      setAuthMode("existing");
      void qc.invalidateQueries({ queryKey: ["catalog", workspace] });
      void qc.invalidateQueries({ queryKey: ["credentials", workspace] });
      void qc.invalidateQueries({ queryKey: ["provider-connections", workspace] });
    },
  });

  const reusableCredentials = credentials.filter((credential) =>
    credentialReusableForProvider(credential, selectedDescriptor),
  );
  const requiredConfigurationMissing = (
    selectedDescriptor?.configuration_fields ?? []
  )
    .filter((field) => field.kind !== "secret" && field.required)
    .some((field) => {
      if (field.key === "base_url") return !draft.baseUrl.trim();
      return !configuration[field.key]?.trim();
    });
  const newAuthenticationReady =
    authMode === "oauth" || (authMode === "api_key" && !!apiKey);
  const connectionReady =
    !!selectedDescriptor &&
    !requiredConfigurationMissing &&
    (authMode === "existing" ? !!syncCredential : newAuthenticationReady);

  return (
    <>
      <Card>
        <h2>{app.t("Provider connections", "供应商连接")}</h2>
        <p className="hint">
          {app.t(
            "Choose a provider and authentication method. Verify & import models checks the credential and endpoint before making the models available to Agents.",
            "选择供应商和认证方式。“验证并导入模型”会先检查凭证与端点，再将模型提供给 Agent。",
          )}
        </p>
        <div className="row" style={{ marginBottom: 14 }}>
          {descriptors.map((descriptor) => {
            const connection = connections.find(
              (item) => item.provider_id === descriptor.provider_kind,
            );
            return (
              <Button
                key={descriptor.provider_kind}
                variant={
                  draft.provider === descriptor.provider_kind ? "primary" : "ghost"
                }
                onClick={() => selectDescriptor(descriptor)}
              >
                {descriptor.display_name}
                <span className="mut" style={{ marginLeft: 6 }}>
                  {connectionStatusLabel(connection?.status, app.t)}
                  {connection?.active_models
                    ? ` · ${providerModelCountLabel(connection.active_models, app.locale)}`
                    : ""}
                </span>
              </Button>
            );
          })}
        </div>
        <div className="row" style={{ alignItems: "flex-end" }}>
          {(selectedDescriptor?.configuration_fields ?? [])
            .filter((field) => field.kind !== "secret" && !field.advanced)
            .map((field) => (
              <TextField
                key={field.key}
                label={field.label}
                mono
                placeholder={field.placeholder ?? undefined}
                value={
                  field.key === "base_url"
                    ? draft.baseUrl
                    : configuration[field.key] ?? ""
                }
                onChange={(event) => {
                  if (field.key === "base_url") {
                    setDraft({ ...draft, baseUrl: event.target.value });
                  } else {
                    setConfiguration({
                      ...configuration,
                      [field.key]: event.target.value,
                    });
                  }
                }}
              />
            ))}
          <span className="field">
            <label>{app.t("Authentication", "认证方式")}</label>
            <span className="row">
              {(selectedDescriptor?.auth_methods ?? []).map((method) => (
                <Button
                  key={method}
                  variant={authMode === method ? "primary" : "ghost"}
                  onClick={() => {
                    setAuthMode(method);
                    setSyncCredential("");
                  }}
                >
                  {method === "oauth"
                    ? app.t("Use gcloud login", "使用 gcloud 登录")
                    : app.t("New API key", "新 API Key")}
                </Button>
              ))}
              <Button
                variant={authMode === "existing" ? "primary" : "ghost"}
                disabled={reusableCredentials.length === 0}
                onClick={() => setAuthMode("existing")}
              >
                {app.t("Use existing", "复用已有凭证")}
              </Button>
            </span>
          </span>
          {authMode === "api_key" && (
            <>
              <TextField
                label={app.t("Credential name", "凭证名称")}
                value={credentialName}
                placeholder={app.t("Provider · purpose", "供应商 · 用途")}
                onChange={(event) => setCredentialName(event.target.value)}
              />
              <SecretField
                label={app.t("API key (write-only)", "API Key（仅写入）")}
                hasStored={false}
                onChange={(intent) => setApiKey(intent.value ?? "")}
              />
              {selectedDescriptor?.documentation_url && (
                <a
                  className="protocol-help-link"
                  href={selectedDescriptor.documentation_url}
                  target="_blank"
                  rel="noreferrer"
                >
                  {app.t(
                    `Get an API key from ${selectedDescriptor.display_name} ↗`,
                    `前往 ${selectedDescriptor.display_name} 获取 API Key ↗`,
                  )}
                </a>
              )}
            </>
          )}
          {authMode === "oauth" && (
            <span className="field">
              <label>{app.t("OAuth helper", "OAuth 辅助程序")}</label>
              <span className="input mono">gcloud · active account</span>
            </span>
          )}
          {authMode === "existing" && (
            <SelectField
              label={app.t("Existing credential", "已有凭证")}
              value={syncCredential}
              onChange={(event) => setSyncCredential(event.target.value)}
            >
              <option value="">{app.t("Choose credential", "选择凭证")}</option>
              {reusableCredentials.map((credential) => (
                <option key={credential.id} value={credential.id}>
                  {credentialLabel(credential)} · {credential.provider_id ?? app.t("Shared", "通用")}
                </option>
              ))}
            </SelectField>
          )}
          <Button
            variant="primary"
            disabled={!connectionReady || connect.isPending}
            onClick={() => connect.mutate()}
          >
            {connect.isPending
              ? app.t("Verifying…", "正在验证…")
              : app.t("Verify & import models", "验证并导入模型")}
          </Button>
        </div>
        <p className="hint" style={{ marginTop: 10 }}>
          {selectedDescriptor?.supports_model_discovery
            ? app.t(
                "Awaken reads the provider's model directory automatically. The connection, write-only credential, and discovered models are saved only after verification succeeds.",
                "Awaken 会自动读取供应商的模型目录；只有验证成功后，才会保存连接、仅写凭证和发现的模型。",
              )
            : app.t(
                "This provider does not expose automatic model discovery. Verify the connection, then add its supported models manually.",
                "此供应商不提供自动模型发现。请先验证连接，再手动添加其支持的模型。",
              )}
        </p>
        <details style={{ marginTop: 12 }}>
          <summary className="mut">
            {app.t("Advanced connection settings", "高级连接设置")}
          </summary>
          <div className="row" style={{ marginTop: 10, alignItems: "flex-end" }}>
            <TextField
              label={app.t("Endpoint name (optional)", "端点名称（可选）")}
              mono
              placeholder={app.t(
                "Only for a second endpoint using this dialect",
                "仅用于同一方言的第二个端点",
              )}
              value={draft.endpointName}
              onChange={(event) =>
                setDraft({ ...draft, endpointName: event.target.value })
              }
            />
            <SelectField
              label={app.t("API format", "API 格式")}
              value={draft.dialect}
              onChange={(event) => {
                const dialect = event.target.value;
                const endpoint = selectedDescriptor?.default_endpoints.find(
                  (candidate) => candidate.dialect === dialect,
                );
                setDraft({ ...draft, dialect, baseUrl: endpoint?.base_url ?? draft.baseUrl });
              }}
            >
              {(selectedDescriptor?.supported_dialects ?? [draft.dialect]).map(
                (dialect) => (
                  <option key={dialect} value={dialect}>
                    {dialectLabel(dialect, app.t)}
                  </option>
                ),
              )}
            </SelectField>
            {(selectedDescriptor?.configuration_fields ?? [])
              .filter((field) => field.kind !== "secret" && field.advanced)
              .map((field) => (
                <TextField
                  key={field.key}
                  label={field.label}
                  mono
                  placeholder={field.placeholder ?? undefined}
                  value={
                    field.key === "base_url"
                      ? draft.baseUrl
                      : configuration[field.key] ?? ""
                  }
                  onChange={(event) => {
                    if (field.key === "base_url") {
                      setDraft({ ...draft, baseUrl: event.target.value });
                    } else {
                      setConfiguration({
                        ...configuration,
                        [field.key]: event.target.value,
                      });
                    }
                  }}
                />
              ))}
          </div>
        </details>
        {connect.data && (
          <div className="banner info" style={{ marginTop: 12 }}>
            <span>✓</span>
            <span>
              {app.t(
                "Credential verified and models imported",
                "凭证已验证，模型已导入",
              )}{" "}
              · {connect.data.sync.discovered} {app.t("models", "个模型")}
            </span>
          </div>
        )}
        {connect.error instanceof Error && (
          <div className="err">{connect.error.message}</div>
        )}
      </Card>

    </>
  );
}
