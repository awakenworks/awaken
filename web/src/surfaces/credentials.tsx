// Workspace · Inference credentials: the SUPPLY-side sources and pools that
// authenticate the MODEL PROVIDER (secret-in / secret-free-out). This is one of
// two credential axes (design/web-ui.md §1): inference lives here; runtime/tool
// credentials (MCP OAuth, static bearer, env-var) live in the project Vault.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api/client";
import type { CredentialSource, CredentialValidation } from "../lib/api/types";
import { useApp } from "../lib/app-state";

const WORKSPACE = "wrkspc_default";

function SourceRow({ source }: { source: CredentialSource }) {
  const app = useApp();
  const qc = useQueryClient();
  const [model, setModel] = useState("claude-sonnet-4-5");
  const validate = useMutation({
    mutationFn: () =>
      api.post<CredentialValidation>(`/v1/config/credentials/${source.id}/validate`, {
        workspace_id: WORKSPACE,
        model_id: model,
      }),
  });
  const archive = useMutation({
    mutationFn: () => api.post(`/v1/config/credentials/${source.id}/archive`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["credentials"] }),
  });
  const statusTone = source.status === "active" ? "ok" : "neutral";
  return (
    <tr>
      <td className="mono">{source.id}</td>
      <td>
        <span className="pill neutral">{source.kind}</span>
      </td>
      <td>{source.provider_id ?? source.env_key ?? "—"}</td>
      <td>
        <span className={`pill ${statusTone}`}>{source.status}</span>
      </td>
      <td>
        {validate.data && (
          <span
            className={`pill ${
              validate.data.status === "valid" ? "ok" : validate.data.status === "invalid" ? "danger" : "neutral"
            }`}
          >
            {validate.data.status} · {validate.data.adapter_kind}
          </span>
        )}
        {validate.error instanceof Error && <span className="err">{validate.error.message}</span>}
      </td>
      <td style={{ textAlign: "right", whiteSpace: "nowrap" }}>
        <input
          className="input mono"
          style={{ width: 150, height: 26, marginRight: 6 }}
          value={model}
          onChange={(e) => setModel(e.target.value)}
          title={app.t("model to probe with", "用于探针的模型")}
        />
        <button className="btn ghost" style={{ height: 26 }} disabled={validate.isPending} onClick={() => validate.mutate()}>
          {app.t("Validate", "验证")}
        </button>{" "}
        <button
          className="btn ghost"
          style={{ height: 26 }}
          disabled={archive.isPending || source.status !== "active"}
          onClick={() => archive.mutate()}
        >
          {app.t("Archive", "归档")}
        </button>
      </td>
    </tr>
  );
}

export default function CredentialsSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const sources = useQuery({
    queryKey: ["credentials"],
    queryFn: () => api.get<CredentialSource[]>(`/v1/config/credentials?workspace_id=${WORKSPACE}`),
  });
  const [entering, setEntering] = useState(false);
  const [form, setForm] = useState({ kind: "vault", provider: "anthropic", envKey: "", secret: "" });
  const enter = useMutation({
    mutationFn: () =>
      api.post<CredentialSource>("/v1/config/credentials", {
        workspace_id: WORKSPACE,
        kind: form.kind,
        provider_id: form.provider || undefined,
        env_key: form.envKey || undefined,
        secret: form.secret,
      }),
    onSuccess: () => {
      setEntering(false);
      setForm({ ...form, secret: "" });
      void qc.invalidateQueries({ queryKey: ["credentials"] });
    },
  });
  return (
    <>
      <div className="banner info">
        <span>ⓘ</span>
        <span>
          {app.t(
            "Inference credentials only — these authenticate the model provider. Runtime/tool credentials (MCP OAuth, static bearer, env-var) live in the project Vault.",
            "仅推理凭证——用于模型供应商鉴权。运行/工具凭证(MCP OAuth、static bearer、env-var)在 Project Vault。",
          )}
        </span>
      </div>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut">
          {app.t("Supply-side sources — never echoed; validation is a live provider probe.", "供给侧凭证——永不回显;验证是真实的供应商探针。")}
        </span>
        <button className="btn primary" onClick={() => setEntering(true)}>
          + {app.t("Enter credential", "录入凭证")}
        </button>
      </div>
      <div className="card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>ID</th>
              <th>Kind</th>
              <th>Provider / env</th>
              <th>Status</th>
              <th>{app.t("Last probe", "最近探针")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(sources.data ?? []).map((s) => (
              <SourceRow key={s.id} source={s} />
            ))}
            {(sources.data ?? []).length === 0 && (
              <tr>
                <td colSpan={6} className="mut">
                  {app.t("No credential sources in this workspace yet.", "工作区还没有凭证源。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      {sources.error instanceof Error && <div className="err">{sources.error.message}</div>}

      <div className="banner gate">
        <span>ⓘ</span>
        <span>
          {app.t(
            "Pools (ordinal failover) are authored via PUT /v1/config/credential-pools/:id — pool UI lands with the profile editor.",
            "凭证池(ordinal 失效顺位)经 PUT /v1/config/credential-pools/:id 作者化——池 UI 随 profile 编辑器落地。",
          )}
        </span>
      </div>

      {entering && (
        <div className="overlay" onClick={() => setEntering(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3>{app.t("Enter credential", "录入凭证")}</h3>
            <div className="row">
              {["vault", "env"].map((k) => (
                <button key={k} className={`btn ${form.kind === k ? "primary" : "ghost"}`} onClick={() => setForm({ ...form, kind: k })}>
                  {k}
                </button>
              ))}
            </div>
            <div className="field">
              <label>provider_id</label>
              <input className="input mono" value={form.provider} onChange={(e) => setForm({ ...form, provider: e.target.value })} />
            </div>
            {form.kind === "env" ? (
              <div className="field">
                <label>env_key</label>
                <input className="input mono" placeholder="ANTHROPIC_API_KEY" value={form.envKey} onChange={(e) => setForm({ ...form, envKey: e.target.value })} />
              </div>
            ) : (
              <div className="field">
                <label>secret ({app.t("write-only, sealed", "只写,密封")})</label>
                <input className="input mono" type="password" value={form.secret} onChange={(e) => setForm({ ...form, secret: e.target.value })} />
              </div>
            )}
            {enter.error instanceof Error && <div className="err">{enter.error.message}</div>}
            <div className="row" style={{ justifyContent: "flex-end" }}>
              <button className="btn primary" disabled={enter.isPending} onClick={() => enter.mutate()}>
                {app.t("Seal & save", "密封保存")}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
