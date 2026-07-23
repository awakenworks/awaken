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
  });
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
      // The context window is a per-model_id attribute (not an offering field): it feeds
      // the compaction token budget. Publish it only when the operator entered one.
      const ctx = Number(draft.contextWindow);
      if (draft.contextWindow.trim() && Number.isFinite(ctx) && ctx > 0) {
        await api.put(ws(`/v1/config/model-attributes/${draft.model}`), { context_window: Math.round(ctx) });
      }
    },
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["catalog"] }),
  });
  const [syncCredential, setSyncCredential] = useState("");
  const syncModels = useMutation({
    mutationFn: () =>
      api.post<CatalogSyncResult>(ws(`/v1/config/endpoints/${draft.endpoint}/discover-models`), {
        workspace_id: workspace,
        credential_source_id: syncCredential,
      }),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["catalog"] }),
  });
  const [testModel, setTestModel] = useState<string | null>(null);
  const [resolveModel, setResolveModel] = useState("claude-sonnet-4-5");
  const [resolveBinding, setResolveBinding] = useState("");
  const resolve = useMutation({
    mutationFn: () =>
      api.post<ResolvedInferenceView>(ws("/v1/config/inference/resolve"), {
        workspace_id: workspace,
        model_id: resolveModel,
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
                </td>
                <td className="mut">
                  {c?.model_attributes?.[o.model_id]?.context_window
                    ? `${fmtTokens(c.model_attributes[o.model_id].context_window!)}`
                    : "—"}
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
                <td colSpan={7} className="mut">
                  {app.t("Empty catalog — author one below.", "目录为空——在下方作者化。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>

      <Card>
        <h2>{app.t("Author provider / endpoint / offering", "作者化 provider / endpoint / offering")}</h2>
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
          <TextField label="Provider" mono value={draft.provider} onChange={(e) => setDraft({ ...draft, provider: e.target.value })} />
          <TextField label="Endpoint id" mono value={draft.endpoint} onChange={(e) => setDraft({ ...draft, endpoint: e.target.value })} />
          <span className="field" style={{ flex: 1 }}>
            <label>base_url ({app.t("optional", "可选")})</label>
            <input className="input mono" value={draft.baseUrl} onChange={(e) => setDraft({ ...draft, baseUrl: e.target.value })} />
          </span>
          <SelectField label="Dialect" value={draft.dialect} onChange={(e) => setDraft({ ...draft, dialect: e.target.value })}>
            <option value="anthropic_messages">anthropic_messages</option>
            <option value="open_ai_chat">open_ai_chat</option>
            <option value="gemini">gemini</option>
            <option value="vertex_gemini">vertex_gemini</option>
          </SelectField>
          <TextField label="Model id" mono placeholder="model-id" value={draft.model} onChange={(e) => setDraft({ ...draft, model: e.target.value })} />
          <TextField
            label={app.t("Context window", "上下文窗口")}
            mono
            placeholder="200000"
            value={draft.contextWindow}
            onChange={(e) => setDraft({ ...draft, contextWindow: e.target.value })}
          />
          <Button variant="primary" style={{ alignSelf: "flex-end" }} disabled={upsert.isPending} onClick={() => upsert.mutate()}>
            {app.t("Author", "写入")}
          </Button>
        </div>
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
