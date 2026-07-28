import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import {
  Button,
  Card,
  SecretField,
  SelectField,
  TextField,
} from "../components/ui";
import { api, ws } from "../lib/api/client";
import type {
  CatalogSyncResult,
  CredentialSource,
  EnvironmentProviderProposal,
  ProviderConnectionView,
  ProviderConnectionSummary,
  ProviderDriverDescriptor,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";

export interface ProviderProfileSuggestion {
  providerId: string;
  credentialId: string;
  revision: number;
}

interface ProviderConnectionPanelProps {
  credentials: CredentialSource[];
  descriptors: ProviderDriverDescriptor[];
  connections: ProviderConnectionSummary[];
  proposals: EnvironmentProviderProposal[];
  onProfileSuggestion(suggestion: ProviderProfileSuggestion): void;
}

interface ProviderDraft {
  provider: string;
  endpoint: string;
  baseUrl: string;
  model: string;
  dialect: string;
  contextWindow: string;
  maxOutputTokens: string;
}

function parseOptionalTokenLimit(label: string, value: string): number | undefined {
  if (!value.trim()) return undefined;
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`${label} must be a positive integer`);
  }
  return parsed;
}

export function providerDraftDefaults(descriptor: ProviderDriverDescriptor) {
  const endpoint = descriptor.default_endpoints[0];
  return {
    provider: descriptor.provider_kind,
    endpoint: `${descriptor.provider_kind}-${endpoint?.id_suffix ?? "endpoint"}`,
    baseUrl: endpoint?.base_url ?? "",
    dialect: endpoint?.dialect ?? descriptor.supported_dialects[0],
    model: "",
  };
}

export function providerConfigurationDefaults(descriptor: ProviderDriverDescriptor) {
  return Object.fromEntries(
    descriptor.configuration_fields
      .filter((field) => field.kind !== "secret")
      .map((field) => [field.key, field.placeholder ?? ""]),
  );
}

export function providerEndpointCoordinates(
  descriptor: ProviderDriverDescriptor | undefined,
  draft: { endpoint: string; baseUrl: string },
  configuration: Record<string, string>,
) {
  if (descriptor?.provider_kind !== "vertex") {
    return { endpoint: draft.endpoint, baseUrl: draft.baseUrl };
  }
  const project = configuration.project_id?.trim() ?? "";
  const location = configuration.location?.trim() || "global";
  const host =
    location === "global"
      ? "aiplatform.googleapis.com"
      : `${location}-aiplatform.googleapis.com`;
  return {
    endpoint: draft.endpoint || "vertex-gemini",
    baseUrl: project
      ? `https://${host}/v1/projects/${encodeURIComponent(project)}/locations/${encodeURIComponent(location)}/`
      : "",
  };
}

export default function ProviderConnectionPanel({
  credentials,
  descriptors,
  connections,
  proposals,
  onProfileSuggestion,
}: ProviderConnectionPanelProps) {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const [draft, setDraft] = useState<ProviderDraft>({
    provider: "anthropic",
    endpoint: "anthropic-messages",
    baseUrl: "",
    model: "claude-sonnet-4-5",
    dialect: "anthropic_messages",
    contextWindow: "",
    maxOutputTokens: "",
  });
  const [apiKey, setApiKey] = useState("");
  const [authMode, setAuthMode] = useState<"api_key" | "oauth" | "existing">(
    "api_key",
  );
  const [configuration, setConfiguration] = useState<Record<string, string>>({});
  const [syncCredential, setSyncCredential] = useState("");
  const selectedDescriptor = descriptors.find(
    (descriptor) => descriptor.provider_kind === draft.provider,
  );

  const selectDescriptor = (descriptor: ProviderDriverDescriptor) => {
    const existing = credentials.find(
      (credential) =>
        credential.status === "active" &&
        credential.provider_id === descriptor.provider_kind,
    );
    setDraft({
      ...draft,
      ...providerDraftDefaults(descriptor),
      contextWindow: "",
      maxOutputTokens: "",
    });
    setConfiguration(providerConfigurationDefaults(descriptor));
    setApiKey("");
    setAuthMode(existing ? "existing" : descriptor.auth_methods[0] ?? "api_key");
    setSyncCredential(existing?.id ?? "");
  };

  const upsert = useMutation({
    mutationFn: async () => {
      // Manual authoring extends one existing connection. Provider and endpoint
      // ownership stays exclusively with the Provider Connection command.
      await api.post(ws("/v1/config/offerings"), {
        model_id: draft.model,
        provider_id: draft.provider,
        protocol_endpoint_id: draft.endpoint,
        dialect: draft.dialect,
        upstream_model: null,
      });
      const contextWindow = parseOptionalTokenLimit(
        "Context window",
        draft.contextWindow,
      );
      const maxOutputTokens = parseOptionalTokenLimit(
        "Max output tokens",
        draft.maxOutputTokens,
      );
      if (
        contextWindow != null &&
        maxOutputTokens != null &&
        maxOutputTokens > contextWindow
      ) {
        throw new Error("Max output tokens cannot exceed the context window");
      }
      if (contextWindow != null || maxOutputTokens != null) {
        await api.put(ws(`/v1/config/model-attributes/${draft.model}`), {
          ...(contextWindow != null ? { context_window: contextWindow } : {}),
          ...(maxOutputTokens != null ? { max_output_tokens: maxOutputTokens } : {}),
        });
      }
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["catalog"] });
      void qc.invalidateQueries({ queryKey: ["provider-connections", workspace] });
    },
  });

  const connect = useMutation({
    mutationFn: () => {
      const coordinates = providerEndpointCoordinates(
        selectedDescriptor,
        draft,
        configuration,
      );
      return api.post<ProviderConnectionView>(
        ws("/v1/config/provider-connections"),
        {
          workspace_id: workspace,
          provider_id: draft.provider,
          display_name: selectedDescriptor?.display_name ?? draft.provider,
          endpoint_id: coordinates.endpoint,
          dialect: draft.dialect,
          base_url: coordinates.baseUrl || null,
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
      setApiKey("");
      setSyncCredential(connection.credential.id);
      setAuthMode("existing");
      void qc.invalidateQueries({ queryKey: ["catalog"] });
      void qc.invalidateQueries({ queryKey: ["credentials", workspace] });
      void qc.invalidateQueries({ queryKey: ["provider-connections", workspace] });
    },
  });

  const syncModels = useMutation({
    mutationFn: () =>
      api.post<CatalogSyncResult>(
        ws(`/v1/config/endpoints/${draft.endpoint}/discover-models`),
        {
          workspace_id: workspace,
          credential_source_id: syncCredential,
        },
      ),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["catalog"] });
      void qc.invalidateQueries({ queryKey: ["provider-connections", workspace] });
    },
  });

  const reusableCredentials = credentials.filter(
    (credential) =>
      credential.status === "active" &&
      credential.env_key !== "CLAUDE_CODE_OAUTH_TOKEN" &&
      (credential.provider_id == null ||
        credential.provider_id === draft.provider),
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
    !!draft.endpoint &&
    !requiredConfigurationMissing &&
    (authMode === "existing" ? !!syncCredential : newAuthenticationReady);

  return (
    <>
      <Card>
        <h2>{app.t("Provider connections", "供应商连接")}</h2>
        <p className="hint">
          {app.t(
            "One guided command verifies authentication, saves or reuses one credential, authors the endpoint, and imports the provider's models.",
            "一个引导式命令完成鉴权验证、凭证新建或复用、端点写入和供应商模型导入。",
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
                  {connection?.status ?? "…"}
                  {connection?.active_models
                    ? ` · ${connection.active_models} models`
                    : ""}
                </span>
              </Button>
            );
          })}
        </div>
        {proposals.length > 0 && (
          <div className="banner info" style={{ marginBottom: 12 }}>
            <span>ⓘ</span>
            <span>
              {app.t(
                "Environment discoveries are suggestions only. Choose one to prefill this form; nothing is executable until you explicitly save catalog and vault records.",
                "环境发现仅是建议。选择后只会预填表单；只有显式保存 Catalog 与 Vault 记录后才可执行。",
              )}
              <span className="row" style={{ marginTop: 8 }}>
                {proposals.map((proposal) => (
                  <Button
                    key={proposal.provider_id}
                    variant="ghost"
                    onClick={() =>
                      setDraft({
                        ...draft,
                        provider: proposal.provider_id,
                        endpoint: proposal.endpoint_id,
                        baseUrl: proposal.base_url ?? "",
                        model: proposal.model_id ?? "",
                        dialect: proposal.dialect,
                      })
                    }
                  >
                    {proposal.provider_id} · {proposal.credential_env}
                    {proposal.credential_present ? " ✓" : ""}
                  </Button>
                ))}
              </span>
            </span>
          </div>
        )}
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
            <SecretField
              label={app.t("API key (write-only)", "API Key（仅写入）")}
              hasStored={false}
              onChange={(intent) => setApiKey(intent.value ?? "")}
            />
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
                  {credential.id} · {credential.kind}
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
        <details style={{ marginTop: 12 }}>
          <summary className="mut">
            {app.t("Advanced connection settings", "高级连接设置")}
          </summary>
          <div className="row" style={{ marginTop: 10, alignItems: "flex-end" }}>
            <TextField
              label="Endpoint id"
              mono
              value={draft.endpoint}
              onChange={(event) =>
                setDraft({ ...draft, endpoint: event.target.value })
              }
            />
            <SelectField
              label="Dialect"
              value={draft.dialect}
              onChange={(event) =>
                setDraft({ ...draft, dialect: event.target.value })
              }
            >
              {(selectedDescriptor?.supported_dialects ?? [draft.dialect]).map(
                (dialect) => (
                  <option key={dialect} value={dialect}>
                    {dialect}
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
              <Button
                variant="ghost"
                style={{ marginLeft: 10 }}
                onClick={() =>
                  onProfileSuggestion({
                    providerId: connect.data.provider.id,
                    credentialId: connect.data.credential.id,
                    revision: Date.now(),
                  })
                }
              >
                {app.t(
                  "Configure as workspace default",
                  "配置为工作区默认",
                )}
              </Button>
            </span>
          </div>
        )}
        {connect.error instanceof Error && (
          <div className="err">{connect.error.message}</div>
        )}
      </Card>

      <Card>
        <details>
          <summary>
            {app.t("Advanced catalog maintenance", "高级目录维护")}
          </summary>
          <p className="hint">
            {app.t(
              "Add a manual model only to the selected existing endpoint, or explicitly refresh that endpoint with an existing credential. Provider and endpoint facts remain owned by Provider Connections.",
              "仅向当前已有端点添加手工模型，或使用已有凭证显式刷新该端点。Provider 与 Endpoint 事实仍由供应商连接统一维护。",
            )}
          </p>
          <div className="row" style={{ alignItems: "flex-end" }}>
            <TextField
              label="Model id"
              mono
              placeholder="model-id"
              value={draft.model}
              onChange={(event) =>
                setDraft({ ...draft, model: event.target.value })
              }
            />
            <TextField
              label={app.t("Context window", "上下文窗口")}
              mono
              placeholder="200000"
              value={draft.contextWindow}
              onChange={(event) =>
                setDraft({ ...draft, contextWindow: event.target.value })
              }
            />
            <TextField
              label={app.t("Max output tokens", "最大输出 token")}
              mono
              placeholder={app.t("unknown", "未知")}
              value={draft.maxOutputTokens}
              onChange={(event) =>
                setDraft({ ...draft, maxOutputTokens: event.target.value })
              }
            />
            <Button
              disabled={!draft.model || upsert.isPending}
              onClick={() => upsert.mutate()}
            >
              {app.t("Add model to endpoint", "添加模型到端点")}
            </Button>
            <SelectField
              label={app.t("Refresh with credential", "使用凭证刷新")}
              value={syncCredential}
              onChange={(event) => setSyncCredential(event.target.value)}
            >
              <option value="">{app.t("Choose credential", "选择凭证")}</option>
              {reusableCredentials.map((credential) => (
                <option key={credential.id} value={credential.id}>
                  {credential.id} · {credential.kind}
                </option>
              ))}
            </SelectField>
            <Button
              disabled={
                !syncCredential || !draft.endpoint || syncModels.isPending
              }
              onClick={() => syncModels.mutate()}
            >
              {app.t("Refresh models", "刷新模型")}
            </Button>
          </div>
          {upsert.error instanceof Error && (
            <div className="err">{upsert.error.message}</div>
          )}
          {syncModels.error instanceof Error && (
            <div className="err">{syncModels.error.message}</div>
          )}
        </details>
      </Card>
    </>
  );
}
