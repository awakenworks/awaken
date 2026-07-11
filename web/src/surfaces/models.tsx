// Workspace · Models: the three-layer catalog (Provider → ProtocolEndpoint →
// Offering) plus inference profiles and the dry-run resolve chain.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button, Card, Pill, SelectField, TextField } from "../components/ui";
import { api } from "../lib/api/client";
import type { ProviderCatalog, ResolvedInferenceView } from "../lib/api/types";
import { useApp } from "../lib/app-state";

const WORKSPACE = "wrkspc_default";

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
  const qc = useQueryClient();
  const catalog = useQuery({
    queryKey: ["catalog"],
    queryFn: () => api.get<ProviderCatalog>("/v1/config/catalog"),
  });
  const [draft, setDraft] = useState({
    provider: "anthropic",
    endpoint: "anthropic-messages",
    baseUrl: "",
    model: "claude-sonnet-4-5",
    flavor: "anthropic_messages",
  });
  const upsert = useMutation({
    mutationFn: async () => {
      await api.put(`/v1/config/providers/${draft.provider}`, {
        id: draft.provider,
        slug: draft.provider,
        display_name: draft.provider,
        version: 1,
      });
      await api.put(`/v1/config/endpoints/${draft.endpoint}`, {
        id: draft.endpoint,
        provider_id: draft.provider,
        flavor: draft.flavor,
        base_url: draft.baseUrl || null,
        timeout_secs: 60,
        display_name: draft.endpoint,
        version: 1,
      });
      await api.post("/v1/config/offerings", {
        model_id: draft.model,
        provider_id: draft.provider,
        protocol_endpoint_id: draft.endpoint,
        flavor: draft.flavor,
        upstream_model: null,
      });
    },
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["catalog"] }),
  });
  const [resolveModel, setResolveModel] = useState("claude-sonnet-4-5");
  const [resolveBinding, setResolveBinding] = useState("");
  const resolve = useMutation({
    mutationFn: () =>
      api.post<ResolvedInferenceView>("/v1/config/inference/resolve", {
        workspace_id: WORKSPACE,
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
              <th>Flavor</th>
            </tr>
          </thead>
          <tbody>
            {(c?.offerings ?? []).map((o, i) => (
              <tr key={`${o.model_id}-${i}`}>
                <td className="mono">{o.model_id}</td>
                <td>{o.provider_id}</td>
                <td className="mono mut">{o.protocol_endpoint_id}</td>
                <td>
                  <Pill tone="neutral">{o.flavor}</Pill>
                </td>
              </tr>
            ))}
            {(c?.offerings ?? []).length === 0 && (
              <tr>
                <td colSpan={4} className="mut">
                  {app.t("Empty catalog — author one below.", "目录为空——在下方作者化。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>

      <Card>
        <h2>{app.t("Author provider / endpoint / offering", "作者化 provider / endpoint / offering")}</h2>
        <div className="row">
          <TextField label="Provider" mono value={draft.provider} onChange={(e) => setDraft({ ...draft, provider: e.target.value })} />
          <TextField label="Endpoint id" mono value={draft.endpoint} onChange={(e) => setDraft({ ...draft, endpoint: e.target.value })} />
          <span className="field" style={{ flex: 1 }}>
            <label>base_url ({app.t("optional", "可选")})</label>
            <input className="input mono" value={draft.baseUrl} onChange={(e) => setDraft({ ...draft, baseUrl: e.target.value })} />
          </span>
          <SelectField label="Flavor" value={draft.flavor} onChange={(e) => setDraft({ ...draft, flavor: e.target.value })}>
            <option value="anthropic_messages">anthropic_messages</option>
            <option value="open_ai_chat">open_ai_chat</option>
            <option value="gemini">gemini</option>
          </SelectField>
          <TextField label="Model id" mono value={draft.model} onChange={(e) => setDraft({ ...draft, model: e.target.value })} />
          <Button variant="primary" style={{ alignSelf: "flex-end" }} disabled={upsert.isPending} onClick={() => upsert.mutate()}>
            {app.t("Author", "写入")}
          </Button>
        </div>
        {upsert.error instanceof Error && <div className="err">{upsert.error.message}</div>}
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
    </>
  );
}
