// Workspace Models: Provider → ProtocolEndpoint → Offering, profiles and dry-run resolve.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Fragment, useEffect, useRef, useState } from "react";
import Transcript from "../components/session/Transcript";
import { Button, Card, Modal, Pill, Skeleton, UsageBadges } from "../components/ui";
import { api, IdempotencyScope, workspaceQuery, ws } from "../lib/api/client";
import type {
  AgentConfig,
  CatalogSyncResult,
  CredentialSource,
  ProviderCatalog,
  ProviderConnectionSummary,
  ProviderDriverDescriptor,
  Session,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useConfigCapabilities } from "../lib/useConfigCapabilities";
import {
  cloudModelUiState,
  CloudModelBadge,
  CloudModelNotice,
  CloudModelRefresh,
  modelCatalogPresentation,
} from "./model-cloud-capability";
import ProviderConnectionPanel from "./provider-connection-panel";

/** Compact a token count for the catalog list: 200000 → "200k", 1_000_000 → "1M". */
function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n % 1_000_000 ? 1 : 0)}M`;
  if (n >= 1_000) return `${Math.round(n / 1_000)}k`;
  return String(n);
}

function fmtObservedAt(timestamp: number): string {
  return new Date(timestamp).toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

interface CloudLoginStatus {
  state: "sign_in_required" | "authorizing" | "authenticated" | "failed";
  authorize_url?: string;
  error_code?: string;
}

function dialectLabel(dialect: string, zh: boolean): string {
  const labels: Record<string, [string, string]> = {
    anthropic_messages: ["Anthropic Messages", "Anthropic Messages 格式"],
    open_ai_responses: ["OpenAI Responses", "OpenAI Responses 格式"],
    open_ai_chat: ["OpenAI Chat Completions", "OpenAI Chat Completions 格式"],
    gemini: ["Gemini API", "Gemini API 格式"],
    vertex_gemini: ["Vertex AI Gemini", "Vertex AI Gemini 格式"],
  };
  return labels[dialect]?.[zh ? 1 : 0] ?? dialect;
}

function fnv1a64(value: string): string {
  let hash = 0xcbf29ce484222325n;
  for (const byte of new TextEncoder().encode(value)) {
    hash ^= BigInt(byte);
    hash = BigInt.asUintN(64, hash * 0x100000001b3n);
  }
  return hash.toString(16).padStart(16, "0");
}

export function modelTestAgentId(model: string): string {
  return `awaken-model-test-${fnv1a64(model)}`;
}

export function modelTestAgentConfig(model: string): AgentConfig {
  return {
    id: modelTestAgentId(model),
    name: `Model test · ${model}`,
    description: "Internal Agent used to verify a published model connection.",
    model: { id: model },
    system: "Reply briefly to verify this model connection.",
    metadata: { "awaken.internal": "model-test" },
    tools: [],
    mcp_servers: [],
    skills: [],
    max_steps: 2,
    plugins: [],
    plugin_config: {},
    context_policy: { kind: "keep_all" },
  };
}

/** A live model test. Runtime refuses to stitch an arbitrary model id onto a
 * different Agent's published backend/credential pins, so each model gets one
 * deterministic internal publication and then uses the real durable Session. */
function TestChat({ model }: { model: string }) {
  const app = useApp();
  const [sid, setSid] = useState<string | null>(null);
  const [latencyMs, setLatencyMs] = useState<number | undefined>();
  const started = useRef(false);
  const createIdentity = useRef(new IdempotencyScope("model-test-session-create"));
  const start = useMutation({
    mutationFn: async () => {
      const config = modelTestAgentConfig(model);
      const saved = await api.put<{ id: string; generation: number }>(
        ws(`/v1/config/agents/${config.id}`),
        config,
      );
      const validation = await api.post<{
        valid: boolean;
        issues?: Array<{ path: string; message: string }>;
      }>(ws(`/v1/config/agents/${config.id}/validate`), config);
      if (!validation.valid) {
        throw new Error(validation.issues?.[0]?.message ?? "Model test Agent is not valid.");
      }
      await api.post(
        ws(`/v1/config/agents/${config.id}/publish`),
        { source_revision: saved.generation, resource_revision: 0 },
      );
      const request = {
        agent: config.id,
        title: `test · ${model}`,
        metadata: { "awaken.session.origin": "model-test" },
      };
      return api.post<Session>(
        ws("/v1/sessions"),
        request,
        createIdentity.current.headersFor(request),
      );
    },
    onSuccess: (s) => {
      createIdentity.current.complete();
      setSid(s.id);
    },
  });
  useEffect(() => {
    if (!started.current) {
      started.current = true;
      start.mutate();
    }
  }, [start]);
  const session = useQuery({
    queryKey: ["test-session", sid],
    enabled: !!sid,
    queryFn: () => api.get<Session>(ws(`/v1/sessions/${sid}`)),
    refetchInterval: 4_000,
  });
  if (start.error instanceof Error) {
    return (
      <div className="banner gate">
        <span>!</span>
        <span style={{ flex: 1 }}>
          <strong>{app.t("Connection test could not start", "连接测试无法启动")}</strong>
          <small>{start.error.message}</small>
        </span>
        <Button
          disabled={start.isPending}
          onClick={() => {
            start.reset();
            start.mutate();
          }}
        >
          {app.t("Retry", "重试")}
        </Button>
      </div>
    );
  }
  if (!sid) {
    return (
      <div>
        <Skeleton height={60} />
        <p className="hint">{app.t("Starting a secure test session…", "正在启动安全测试会话…")}</p>
      </div>
    );
  }
  return (
    <div className="model-test-chat">
      <div className="row" style={{ margin: "4px 0 8px" }}>
        <Pill tone="agent">{model}</Pill>
        <UsageBadges usage={session.data?.usage} latencyMs={latencyMs} />
      </div>
      <Transcript
        base={ws(`/v1/sessions/${sid}`)}
        queryKey={["test-events", sid]}
        fixedModel={model}
        autoMessage={{
          id: `model-test-${sid}`,
          text: "Reply with a short confirmation that this model connection is working.",
        }}
        placeholder={app.t("Say hello…", "打个招呼…")}
        onLatency={setLatencyMs}
      />
    </div>
  );
}

export default function ModelsSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const catalog = useQuery({
    queryKey: ["catalog", workspace],
    queryFn: () => api.get<ProviderCatalog>(ws("/v1/config/catalog")),
  });
  const capabilities = useConfigCapabilities();
  const descriptors = useQuery({
    queryKey: ["provider-descriptors", workspace],
    queryFn: () => api.get<ProviderDriverDescriptor[]>(ws("/v1/config/provider-descriptors")),
    staleTime: Infinity,
    enabled: capabilities.data?.models.byok_enabled === true,
  });
  const connections = useQuery({
    queryKey: ["provider-connections", workspace],
    queryFn: () =>
      api.get<ProviderConnectionSummary[]>(
        ws(workspaceQuery("/v1/config/provider-connections", workspace)),
      ),
    enabled: capabilities.data?.models.byok_enabled === true,
  });
  const credentials = useQuery({
    queryKey: ["credentials", workspace],
    queryFn: () => api.get<CredentialSource[]>(ws(workspaceQuery("/v1/config/credentials", workspace))),
    enabled: capabilities.data?.models.byok_enabled === true,
  });
  const [testModel, setTestModel] = useState<string | null>(null);
  const refreshCloudModels = useMutation({
    mutationFn: () =>
      api.post<CatalogSyncResult>(ws("/v1/config/brokered-models/refresh")),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["catalog", workspace] });
      void qc.invalidateQueries({ queryKey: ["provider-connections", workspace] });
    },
  });

  const c = catalog.data;
  const cloudState = cloudModelUiState(capabilities.data);
  const loginWindow = useRef<Window | null>(null);
  const loginUrlApplied = useRef(false);
  const loginCatalogRefreshed = useRef(false);
  const cloudLogin = useQuery({
    queryKey: ["cloud-login", workspace],
    queryFn: () => api.get<CloudLoginStatus>(ws("/v1/config/cloud-login")),
    enabled:
      cloudState === "sign_in_required" &&
      capabilities.data?.identity.cloud_login_enabled === true,
    refetchInterval: (query) =>
      query.state.data?.state === "authenticated" ? false : 750,
  });
  const startCloudLogin = useMutation({
    mutationFn: () => api.post<CloudLoginStatus>(ws("/v1/config/cloud-login")),
    onSuccess: (status) => {
      qc.setQueryData(["cloud-login", workspace], status);
      if (status.authorize_url && loginWindow.current && !loginUrlApplied.current) {
        loginWindow.current.location.href = status.authorize_url;
        loginUrlApplied.current = true;
      }
    },
  });
  useEffect(() => {
    const status = cloudLogin.data;
    if (status?.authorize_url && loginWindow.current && !loginUrlApplied.current) {
      loginWindow.current.location.href = status.authorize_url;
      loginUrlApplied.current = true;
    }
    if (status?.state === "authenticated" && !loginCatalogRefreshed.current) {
      loginCatalogRefreshed.current = true;
      void qc.invalidateQueries({ queryKey: ["config-capabilities", workspace] });
      refreshCloudModels.mutate();
    }
  }, [cloudLogin.data, qc, refreshCloudModels, workspace]);
  const beginCloudLogin = () => {
    loginUrlApplied.current = false;
    loginCatalogRefreshed.current = false;
    loginWindow.current = window.open("about:blank", "awaken-cloud-login");
    startCloudLogin.mutate();
  };
  const managedSupply = cloudState === "managed";
  const presentation = modelCatalogPresentation(cloudState);
  const byokEnabled = capabilities.data?.models.byok_enabled === true;
  const offeringsByProvider = new Map<string, ProviderCatalog["offerings"]>();
  for (const offering of c?.offerings ?? []) {
    const group = offeringsByProvider.get(offering.provider_id) ?? [];
    group.push(offering);
    offeringsByProvider.set(offering.provider_id, group);
  }
  return (
    <>
      {byokEnabled && (descriptors.isPending || credentials.isPending || connections.isPending || catalog.isPending ? (
        <Card>
          <h2>{app.t("Provider connections", "供应商连接")}</h2>
          <Skeleton height={84} />
        </Card>
      ) : descriptors.isError || credentials.isError || connections.isError || catalog.isError ? (
        <Card>
          <h2>{app.t("Provider connections", "供应商连接")}</h2>
          <div className="empty-state error-text" role="alert">
            {app.t(
              "Provider setup could not load. Your existing configuration is unchanged.",
              "无法加载供应商配置。已有配置未发生变化。",
            )}
          </div>
          <Button onClick={() => void descriptors.refetch()}>
            {app.t("Try again", "重试")}
          </Button>
        </Card>
      ) : (
        <ProviderConnectionPanel
          catalog={c}
          credentials={credentials.data ?? []}
          descriptors={descriptors.data}
          connections={connections.data ?? []}
        />
      ))}

      <Card style={{ padding: 0 }}>
        <div className="row" style={{ padding: "13px 16px", justifyContent: "space-between" }}>
          <div className="row">
          <h2 style={{ margin: 0, fontSize: 14 }}>{app.t("Catalog", "模型目录")}</h2>
          <CloudModelBadge state={cloudState} />
          <span className="mut">
            {c
              ? managedSupply
                ? app.t(
                    `${Object.keys(c.providers).length} providers · ${c.offerings.length} models`,
                    `${Object.keys(c.providers).length} 个供应商 · ${c.offerings.length} 个模型`,
                  )
                : app.t(
                    `${Object.keys(c.providers).length} providers · ${Object.keys(c.endpoints).length} endpoints · ${c.offerings.length} models`,
                    `${Object.keys(c.providers).length} 个供应商 · ${Object.keys(c.endpoints).length} 个端点 · ${c.offerings.length} 个模型`,
                  )
              : "…"}
          </span>
          </div>
          <CloudModelRefresh
            state={cloudState}
            pending={refreshCloudModels.isPending}
            onRefresh={() => refreshCloudModels.mutate()}
          />
        </div>
        <CloudModelNotice
          state={cloudState}
          loginPending={
            startCloudLogin.isPending || cloudLogin.data?.state === "authorizing"
          }
          onSignIn={
            capabilities.data?.identity.cloud_login_enabled ? beginCloudLogin : undefined
          }
        />
        {(startCloudLogin.isError || cloudLogin.data?.state === "failed") && (
          <div className="err" role="alert" style={{ margin: "0 16px 12px" }}>
            {app.t(
              "Awaken Cloud sign-in could not be completed. Try again.",
              "无法完成 Awaken Cloud 登录，请重试。",
            )}
          </div>
        )}
        {refreshCloudModels.error instanceof Error && (
          <div className="err" style={{ margin: "0 16px 12px" }}>
            {refreshCloudModels.error.message}
          </div>
        )}
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Model", "模型")}</th>
              {presentation.showSupplyInfrastructure && <th>{app.t("Provider", "供应商")}</th>}
              {presentation.showSupplyInfrastructure && <th>{app.t("Endpoint", "端点")}</th>}
              {presentation.showSupplyInfrastructure && <th>{app.t("API format", "API 格式")}</th>}
              <th>{app.t("Status", "状态")}</th>
              <th>{app.t("Context", "上下文")}</th>
              <th>{app.t("Max output", "最大输出")}</th>
              {presentation.allowSessionTest && <th style={{ textAlign: "right" }}></th>}
            </tr>
          </thead>
          <tbody>
            {[...offeringsByProvider.entries()]
              .sort(([left], [right]) => left.localeCompare(right))
              .map(([providerId, offerings]) => (
              <Fragment key={providerId}>
                <tr>
                  <td colSpan={managedSupply ? 4 : 8} style={{ fontWeight: 650, background: "var(--surface-subtle)" }}>
                    {c?.providers?.[providerId]?.display_name ?? providerId}
                    {!managedSupply && <span className="mut" style={{ marginLeft: 8 }}>{providerId}</span>}
                  </td>
                </tr>
                {offerings.map((o, i) => (
              <tr key={`${o.model_id}-${o.protocol_endpoint_id}-${i}`}>
                <td className="mono">{o.model_id}</td>
                {presentation.showSupplyInfrastructure && <td>{o.provider_id}</td>}
                {presentation.showSupplyInfrastructure && <td className="mono mut">{o.protocol_endpoint_id}</td>}
                {presentation.showSupplyInfrastructure && <td>
                  <Pill tone="neutral">{dialectLabel(o.dialect, app.locale === "zh")}</Pill>
                </td>}
                <td>
                  <Pill tone={(o.status ?? "active") === "active" ? "agent" : "neutral"}>
                    {(o.status ?? "active") === "active" ? app.t("Available", "可用") : app.t("Unavailable", "不可用")}
                    {!managedSupply && (
                      <> · {(o.source ?? "manual") === "provider_api" ? app.t("Provider sync", "供应商同步") : app.t("Added manually", "手动添加")}</>
                    )}
                  </Pill>
                  {!managedSupply && (o.source ?? "manual") === "provider_api" && (
                    <div
                      className="mut"
                      title={
                        o.last_seen_at_unix_ms
                          ? new Date(o.last_seen_at_unix_ms).toISOString()
                          : app.t("No successful observation recorded", "尚无成功发现记录")
                      }
                      style={{ marginTop: 4, fontSize: 11 }}
                    >
                      {o.last_seen_at_unix_ms
                        ? `${app.t("last seen", "上次发现")} ${fmtObservedAt(o.last_seen_at_unix_ms)}`
                        : app.t("last seen unknown", "上次发现时间未知")}
                    </div>
                  )}
                </td>
                <td className="mut">
                  {c?.model_attributes?.[o.model_id]?.context_window
                    ? `${fmtTokens(c.model_attributes[o.model_id].context_window!)}`
                    : "—"}
                  {c?.model_attributes?.[o.model_id]?.provenance?.context_window?.source && (
                    <div style={{ fontSize: 11 }}>
                      {c.model_attributes[o.model_id].provenance!.context_window.source}
                    </div>
                  )}
                </td>
                <td className="mut">
                  {c?.model_attributes?.[o.model_id]?.max_output_tokens
                    ? `${fmtTokens(c.model_attributes[o.model_id].max_output_tokens!)}`
                    : "—"}
                  {c?.model_attributes?.[o.model_id]?.provenance?.max_output_tokens?.source && (
                    <div style={{ fontSize: 11 }}>
                      {c.model_attributes[o.model_id].provenance!.max_output_tokens.source}
                    </div>
                  )}
                </td>
                {presentation.allowSessionTest && <td style={{ textAlign: "right" }}>
                  <Button
                    style={{ height: 24 }}
                    disabled={(o.status ?? "active") !== "active"}
                    onClick={() => setTestModel(o.model_id)}
                  >
                    {app.t("Test", "测试")}
                  </Button>
                </td>}
              </tr>
                ))}
              </Fragment>
            ))}
            {(c?.offerings ?? []).length === 0 && (
              <tr>
                <td colSpan={managedSupply ? 4 : 8} className="mut">
                  {app.t(
                    byokEnabled
                      ? "No models yet. Complete the provider connection above, then verify and import its models."
                      : "No managed models are currently available. Contact your workspace administrator.",
                    byokEnabled
                      ? "还没有模型。请先完成上方供应商连接，再验证并导入模型。"
                      : "当前没有可用的托管模型，请联系 Workspace 管理员。",
                  )}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>

      {presentation.allowSessionTest && testModel && (
        <Modal
          title={app.t(`Test model · ${testModel}`, `测试模型 · ${testModel}`)}
          onClose={() => setTestModel(null)}
          width="min(680px, 94vw)"
          className="model-test-modal"
        >
          <p className="hint">
            {app.t(
              "A live round-trip through the resolve chain — a real reply proves the connection.",
              "一次经过 resolve 链的真实往返——真实回复即证明连接可用。",
            )}
          </p>
          <TestChat model={testModel} />
        </Modal>
      )}
    </>
  );
}
