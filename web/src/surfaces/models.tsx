// Workspace · Models: the three-layer catalog (Provider → ProtocolEndpoint →
// Offering) plus inference profiles and the dry-run resolve chain.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import Transcript from "../components/session/Transcript";
import { Button, Card, Modal, Pill, SelectField, Skeleton, TextField, UsageBadges } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type {
  CatalogSyncResult,
  CredentialSource,
  EnvironmentProviderProposal,
  ProviderCatalog,
  ProviderConnectionView,
  ProviderConnectionSummary,
  ProviderDriverDescriptor,
  ResolvedInferenceView,
  Session,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";

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

/** A live model test: a scratch session (via the `default` agent) pinned to the
 * chosen model, so a real reply proves the connection — the same transcript
 * engine as the Sandbox. No provider key → the run errors honestly, not a stub. */
function TestChat({ model }: { model: string }) {
  const app = useApp();
  const [sid, setSid] = useState<string | null>(null);
  const [latencyMs, setLatencyMs] = useState<number | undefined>();
  const started = useRef(false);
  const start = useMutation({
    mutationFn: () =>
      api.post<Session>(ws("/v1/sessions"), { agent: "default", title: `test · ${model}` }),
    onSuccess: (s) => setSid(s.id),
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
  if (!sid) return <Skeleton height={60} />;
  return (
    <>
      <div className="row" style={{ margin: "4px 0 8px" }}>
        <Pill tone="agent">{model}</Pill>
        <UsageBadges usage={session.data?.usage} latencyMs={latencyMs} />
      </div>
      <Transcript
        base={ws(`/v1/sessions/${sid}`)}
        queryKey={["test-events", sid]}
        fixedModel={model}
        placeholder={app.t("Say hello…", "打个招呼…")}
        onLatency={setLatencyMs}
      />
    </>
  );
}

function ResolveChain({ view }: { view: ResolvedInferenceView }) {
  return (
    <div className="chain" style={{ marginTop: 10 }}>
      <span className="chip mono">{view.model_id}</span>
      <span className="arrow">→</span>
      <span className="chip">
        credential
        <span className="dot" style={{ background: view.credential_present ? "var(--ok)" : "var(--fg3)" }} />
        {view.credential_present ? "present" : "none"}
      </span>
      <span className="arrow">→</span>
      <span className="chip">
        {view.provider_id}
        <span className="mono mut">{view.adapter_kind}</span>
        <span className="dot" style={{ background: "var(--ok)" }} />
      </span>
      {view.base_url && <code className="mut">{view.base_url}</code>}
    </div>
  );
}

export default function ModelsSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const catalog = useQuery({
    queryKey: ["catalog"],
    queryFn: () => api.get<ProviderCatalog>(ws("/v1/config/catalog")),
  });
  const proposals = useQuery({
    queryKey: ["provider-proposals"],
    queryFn: () => api.get<EnvironmentProviderProposal[]>(ws("/v1/config/provider-proposals")),
  });
  const descriptors = useQuery({
    queryKey: ["provider-descriptors"],
    queryFn: () => api.get<ProviderDriverDescriptor[]>(ws("/v1/config/provider-descriptors")),
    staleTime: Infinity,
  });
  const connections = useQuery({
    queryKey: ["provider-connections", workspace],
    queryFn: () =>
      api.get<ProviderConnectionSummary[]>(
        ws(`/v1/config/provider-connections?workspace_id=${workspace}`),
      ),
  });
  const credentials = useQuery({
    queryKey: ["credentials", workspace],
    queryFn: () => api.get<CredentialSource[]>(ws(`/v1/config/credentials?workspace_id=${workspace}`)),
  });
  const [draft, setDraft] = useState({
    provider: "anthropic",
    endpoint: "anthropic-messages",
    baseUrl: "",
    model: "claude-sonnet-4-5",
    dialect: "anthropic_messages",
    contextWindow: "",
    maxOutputTokens: "",
  });
  const [apiKey, setApiKey] = useState("");
  const selectedDescriptor = descriptors.data?.find(
    (descriptor) => descriptor.provider_kind === draft.provider,
  );
  const selectDescriptor = (descriptor: ProviderDriverDescriptor) => {
    setDraft({
      ...draft,
      ...providerDraftDefaults(descriptor),
    });
  };
  const upsert = useMutation({
    mutationFn: async () => {
      await api.put(ws(`/v1/config/providers/${draft.provider}`), {
        id: draft.provider,
        slug: draft.provider,
        display_name: draft.provider,
        version: 1,
      });
      await api.put(ws(`/v1/config/endpoints/${draft.endpoint}`), {
        id: draft.endpoint,
        provider_id: draft.provider,
        dialect: draft.dialect,
        base_url: draft.baseUrl || null,
        timeout_secs: 60,
        display_name: draft.endpoint,
        version: 1,
      });
      await api.post(ws("/v1/config/offerings"), {
        model_id: draft.model,
        provider_id: draft.provider,
        protocol_endpoint_id: draft.endpoint,
        dialect: draft.dialect,
        upstream_model: null,
      });
      // Token limits are optional per-model_id facts, not connection prerequisites.
      // Omitted fields remain explicitly unknown; the server stamps their provenance.
      const contextWindow = parseOptionalTokenLimit("Context window", draft.contextWindow);
      const maxOutputTokens = parseOptionalTokenLimit("Max output tokens", draft.maxOutputTokens);
      if (contextWindow != null && maxOutputTokens != null && maxOutputTokens > contextWindow) {
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
    mutationFn: () =>
      api.post<ProviderConnectionView>(ws("/v1/config/provider-connections"), {
        workspace_id: workspace,
        provider_id: draft.provider,
        display_name: selectedDescriptor?.display_name ?? draft.provider,
        endpoint_id: draft.endpoint,
        dialect: draft.dialect,
        base_url: draft.baseUrl || null,
        timeout_secs: 60,
        secret: apiKey,
      }),
    onSuccess: (connection) => {
      setApiKey("");
      setSyncCredential(connection.credential.id);
      void qc.invalidateQueries({ queryKey: ["catalog"] });
      void qc.invalidateQueries({ queryKey: ["credentials", workspace] });
      void qc.invalidateQueries({ queryKey: ["provider-connections", workspace] });
    },
  });
  const [syncCredential, setSyncCredential] = useState("");
  const syncModels = useMutation({
    mutationFn: () =>
      api.post<CatalogSyncResult>(ws(`/v1/config/endpoints/${draft.endpoint}/discover-models`), {
        workspace_id: workspace,
        credential_source_id: syncCredential,
      }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["catalog"] });
      void qc.invalidateQueries({ queryKey: ["provider-connections", workspace] });
    },
  });
  const [testModel, setTestModel] = useState<string | null>(null);
  const [resolveModel, setResolveModel] = useState("claude-sonnet-4-5");
  const [resolveProvider, setResolveProvider] = useState("anthropic");
  const [resolveEndpoint, setResolveEndpoint] = useState("anthropic-messages");
  const [resolveBinding, setResolveBinding] = useState("");
  const resolve = useMutation({
    mutationFn: () =>
      api.post<ResolvedInferenceView>(ws("/v1/config/inference/resolve"), {
        workspace_id: workspace,
        target: {
          model_id: resolveModel,
          provider_id: resolveProvider,
          protocol_endpoint_id: resolveEndpoint,
        },
        binding: resolveBinding
          ? { type: "exact", credential_source_id: resolveBinding }
          : { type: "none" },
      }),
  });

  const c = catalog.data;
  return (
    <>
      <Card style={{ padding: 0 }}>
        <div className="row" style={{ padding: "13px 16px" }}>
          <h2 style={{ margin: 0, fontSize: 14 }}>{app.t("Catalog", "模型目录")}</h2>
          <span className="mut">
            {c
              ? `${Object.keys(c.providers).length} providers · ${Object.keys(c.endpoints).length} endpoints · ${c.offerings.length} offerings`
              : "…"}
          </span>
        </div>
        <table className="table">
          <thead>
            <tr>
              <th>Model</th>
              <th>Provider</th>
              <th>Endpoint</th>
              <th>Dialect</th>
              <th>Status</th>
              <th>{app.t("Context", "上下文")}</th>
              <th>{app.t("Max output", "最大输出")}</th>
              <th style={{ textAlign: "right" }}></th>
            </tr>
          </thead>
          <tbody>
            {(c?.offerings ?? []).map((o, i) => (
              <tr key={`${o.model_id}-${i}`}>
                <td className="mono">{o.model_id}</td>
                <td>{o.provider_id}</td>
                <td className="mono mut">{o.protocol_endpoint_id}</td>
                <td>
                  <Pill tone="neutral">{o.dialect}</Pill>
                </td>
                <td>
                  <Pill tone={(o.status ?? "active") === "active" ? "agent" : "neutral"}>
                    {o.status ?? "active"} · {o.source ?? "manual"}
                  </Pill>
                  {(o.source ?? "manual") === "provider_api" && (
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
                <td style={{ textAlign: "right" }}>
                  <Button style={{ height: 24 }} onClick={() => setTestModel(o.model_id)}>
                    {app.t("Test", "测试")}
                  </Button>
                </td>
              </tr>
            ))}
            {(c?.offerings ?? []).length === 0 && (
              <tr>
                <td colSpan={8} className="mut">
                  {app.t("Empty catalog — author one below.", "目录为空——在下方作者化。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>

      <Card>
        <h2>{app.t("Connect a model source", "连接模型来源")}</h2>
        <p className="hint">
          {app.t(
            "Choose a supported provider. Its protocols and default endpoint come from the backend driver descriptor.",
            "选择受支持的供应商；协议与默认端点来自后端驱动描述，而非前端硬编码。",
          )}
        </p>
        <div className="row" style={{ marginBottom: 14 }}>
          {(descriptors.data ?? []).map((descriptor) => (
            (() => {
              const connection = connections.data?.find(
                (item) => item.provider_id === descriptor.provider_kind,
              );
              return (
                <Button
                  key={descriptor.provider_kind}
                  variant={draft.provider === descriptor.provider_kind ? "primary" : "ghost"}
                  onClick={() => selectDescriptor(descriptor)}
                >
                  {descriptor.display_name}
                  <span className="mut" style={{ marginLeft: 6 }}>
                    {connection?.status ?? "…"}
                    {connection?.active_models ? ` · ${connection.active_models} models` : ""}
                  </span>
                </Button>
              );
            })()
          ))}
        </div>
        {(proposals.data ?? []).length > 0 && (
          <div className="banner info" style={{ marginBottom: 12 }}>
            <span>ⓘ</span>
            <span>
              {app.t(
                "Environment discoveries are suggestions only. Choose one to prefill this form; nothing is executable until you explicitly save catalog and vault records.",
                "环境发现仅是建议。选择后只会预填表单；只有显式保存 Catalog 与 Vault 记录后才可执行。",
              )}
              <span className="row" style={{ marginTop: 8 }}>
                {(proposals.data ?? []).map((proposal) => (
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
        <div className="row">
          <TextField label="Provider" mono value={draft.provider} readOnly />
          <TextField label="Endpoint id" mono value={draft.endpoint} onChange={(e) => setDraft({ ...draft, endpoint: e.target.value })} />
          <span className="field" style={{ flex: 1 }}>
            <label>base_url ({app.t("optional", "可选")})</label>
            <input className="input mono" value={draft.baseUrl} onChange={(e) => setDraft({ ...draft, baseUrl: e.target.value })} />
          </span>
          <SelectField label="Dialect" value={draft.dialect} onChange={(e) => setDraft({ ...draft, dialect: e.target.value })}>
            {(selectedDescriptor?.supported_dialects ?? [draft.dialect]).map((dialect) => (
              <option key={dialect} value={dialect}>{dialect}</option>
            ))}
          </SelectField>
          <TextField label="Model id" mono placeholder="model-id" value={draft.model} onChange={(e) => setDraft({ ...draft, model: e.target.value })} />
          <TextField
            label={app.t("Context window", "上下文窗口")}
            mono
            placeholder="200000"
            value={draft.contextWindow}
            onChange={(e) => setDraft({ ...draft, contextWindow: e.target.value })}
          />
          <TextField
            label={app.t("Max output tokens", "最大输出 token")}
            mono
            placeholder={app.t("unknown", "未知")}
            value={draft.maxOutputTokens}
            onChange={(e) => setDraft({ ...draft, maxOutputTokens: e.target.value })}
          />
          {selectedDescriptor?.auth_methods.includes("api_key") && (
            <TextField
              label={app.t("API key (write-only)", "API Key（仅写入）")}
              type="password"
              autoComplete="new-password"
              value={apiKey}
              onChange={(event) => setApiKey(event.target.value)}
            />
          )}
          <Button
            variant="primary"
            style={{ alignSelf: "flex-end" }}
            disabled={!apiKey || !draft.endpoint || connect.isPending}
            onClick={() => connect.mutate()}
          >
            {connect.isPending
              ? app.t("Testing…", "正在测试…")
              : app.t("Test & save", "测试并保存")}
          </Button>
          <Button style={{ alignSelf: "flex-end" }} disabled={!draft.model || upsert.isPending} onClick={() => upsert.mutate()}>
            {app.t("Add manual model", "添加手工模型")}
          </Button>
        </div>
        {connect.data && (
          <div className="banner info" style={{ marginTop: 12 }}>
            <span>✓</span>
            <span>
              {app.t("Tested and saved", "已测试并保存")} · {connect.data.sync.discovered}{" "}
              {app.t("models discovered", "个模型已发现")}
            </span>
          </div>
        )}
        {connect.error instanceof Error && <div className="err">{connect.error.message}</div>}
        {upsert.error instanceof Error && <div className="err">{upsert.error.message}</div>}
        <div className="row" style={{ marginTop: 12 }}>
          <SelectField
            label={app.t("Credential for provider discovery", "用于供应商发现的凭证")}
            value={syncCredential}
            onChange={(event) => setSyncCredential(event.target.value)}
          >
            <option value="">{app.t("Choose credential", "选择凭证")}</option>
            {(credentials.data ?? [])
              .filter(
                (credential) =>
                  credential.status === "active" &&
                  (credential.provider_id == null || credential.provider_id === draft.provider),
              )
              .map((credential) => (
                <option key={credential.id} value={credential.id}>
                  {credential.id} · {credential.kind}
                </option>
              ))}
          </SelectField>
          <Button
            style={{ alignSelf: "flex-end" }}
            disabled={!syncCredential || !draft.endpoint || syncModels.isPending}
            onClick={() => syncModels.mutate()}
          >
            {app.t("Discover models", "发现模型")}
          </Button>
          {syncModels.data && (
            <span className="mut" style={{ alignSelf: "flex-end" }}>
              {syncModels.data.discovered} discovered · {syncModels.data.activated} activated ·{" "}
              {syncModels.data.marked_unavailable} unavailable
            </span>
          )}
        </div>
        {syncModels.error instanceof Error && <div className="err">{syncModels.error.message}</div>}
      </Card>

      <Card>
        <h2>{app.t("Resolve dry-run", "Resolve 试算")}</h2>
        <p className="hint">
          {app.t(
            "Exercises the same resolve path a run uses — secret-free result.",
            "走与真实运行相同的 resolve 路径——结果不含任何秘密。",
          )}
        </p>
        <div className="row">
          <TextField label="Model" mono value={resolveModel} onChange={(e) => setResolveModel(e.target.value)} />
          <TextField label="Provider" mono value={resolveProvider} onChange={(e) => setResolveProvider(e.target.value)} />
          <TextField label="Endpoint" mono value={resolveEndpoint} onChange={(e) => setResolveEndpoint(e.target.value)} />
          <TextField
            label={app.t("Credential source (empty = none)", "凭证源(留空 = none)")}
            mono
            placeholder="cs_…"
            value={resolveBinding}
            onChange={(e) => setResolveBinding(e.target.value)}
          />
          <Button variant="primary" style={{ alignSelf: "flex-end" }} onClick={() => resolve.mutate()}>
            Resolve
          </Button>
        </div>
        {resolve.data && <ResolveChain view={resolve.data} />}
        {resolve.error instanceof Error && <div className="err">{resolve.error.message}</div>}
      </Card>

      {testModel && (
        <Modal
          title={app.t(`Test model · ${testModel}`, `测试模型 · ${testModel}`)}
          onClose={() => setTestModel(null)}
          width="min(680px, 94vw)"
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
