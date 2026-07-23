// Workspace credential sources: one secret-in / secret-free-out materialization
// surface shared by model providers, MCP servers, and A2A remotes. Consumers bind
// the source by id; none of them owns OAuth refresh behavior.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api, ws } from "../lib/api/client";
import type { CredentialSource, CredentialValidation } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Button, Card, Pill, SecretField, TextField } from "../components/ui";

function SourceRow({ source }: { source: CredentialSource }) {
  const app = useApp();
  const qc = useQueryClient();
  const [model, setModel] = useState("claude-sonnet-4-5");
  const validate = useMutation({
    mutationFn: () =>
      api.post<CredentialValidation>(ws(`/v1/config/credentials/${source.id}/validate`), {
        workspace_id: source.workspace_id,
        model_id: model,
      }),
  });
  const archive = useMutation({
    mutationFn: () => api.post(ws(`/v1/config/credentials/${source.id}/archive`)),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["credentials"] }),
  });
  const statusTone = source.status === "active" ? "ok" : "neutral";
  return (
    <tr>
      <td className="mono">{source.id}</td>
      <td>
        <Pill tone="neutral">{source.kind}</Pill>
      </td>
      <td>{source.provider_id ?? source.env_key ?? "—"}</td>
      <td>
        <Pill tone={statusTone}>{source.status}</Pill>
      </td>
      <td>
        {validate.data && (
          <Pill
            tone={validate.data.status === "valid" ? "ok" : validate.data.status === "invalid" ? "danger" : "neutral"}
          >
            {validate.data.status} · {validate.data.adapter_kind}
          </Pill>
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
        <Button
          variant="ghost"
          style={{ height: 26 }}
          disabled={validate.isPending || source.kind === "worker_local"}
          onClick={() => validate.mutate()}
        >
          {source.kind === "worker_local" ? app.t("Worker-reported", "Worker 上报") : app.t("Validate", "验证")}
        </Button>{" "}
        <Button
          variant="ghost"
          style={{ height: 26 }}
          disabled={archive.isPending || source.status !== "active"}
          onClick={() => archive.mutate()}
        >
          {app.t("Archive", "归档")}
        </Button>
      </td>
    </tr>
  );
}

export default function CredentialsSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const sources = useQuery({
    queryKey: ["credentials", workspace],
    queryFn: () => api.get<CredentialSource[]>(ws(`/v1/config/credentials?workspace_id=${workspace}`)),
  });
  const [entering, setEntering] = useState(false);
  const [form, setForm] = useState({ kind: "vault", provider: "anthropic", secret: "", oauthHelper: "gcloud" });
  const enter = useMutation({
    mutationFn: () =>
      api.post<CredentialSource>(ws("/v1/config/credentials"), {
        workspace_id: workspace,
        kind: form.kind,
        provider_id: form.provider || undefined,
        secret: form.kind === "vault" ? form.secret : undefined,
        oauth_helper: form.kind === "oauth" ? form.oauthHelper : undefined,
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
            "One credential source, reusable by Model Providers, MCP servers, and A2A remotes. Consumers store only a binding; secrets never return to the UI.",
            "一个凭证源可被 Model Provider、MCP 和 A2A 复用。消费者只保存 binding，secret 永不回显。",
          )}
        </span>
      </div>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut">
          {app.t("Credential sources — materialized only at the outbound adapter boundary.", "凭证源——仅在出站适配器边界实例化。")}
        </span>
        <Button variant="primary" onClick={() => setEntering(true)}>
          + {app.t("Enter credential", "录入凭证")}
        </Button>
      </div>
      <Card style={{ padding: 0 }}>
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
      </Card>
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
              {["vault", "oauth", "worker_local"].map((k) => (
                <Button key={k} variant={form.kind === k ? "primary" : "ghost"} onClick={() => setForm({ ...form, kind: k })}>
                  {k}
                </Button>
              ))}
            </div>
            <TextField label="provider_id" mono value={form.provider} onChange={(e) => setForm({ ...form, provider: e.target.value })} />
            {form.kind === "oauth" ? (
              <label className="field">
                <span>{app.t("OAuth helper", "OAuth 辅助程序")}</span>
                <select className="input mono" value={form.oauthHelper} onChange={(e) => setForm({ ...form, oauthHelper: e.target.value })}>
                  <option value="gcloud">gcloud · active account</option>
                </select>
                <small className="hint">
                  {app.t(
                    "Awaken stores only the helper id and refreshes a short-lived token when a run starts.",
                    "Awaken 只保存 helper id，并在 run 开始时刷新短期 token。",
                  )}
                </small>
              </label>
            ) : form.kind === "vault" ? (
              // The single secret-entry seam (ADR-0038 invariant: a stored secret is
              // never read back into the UI — write-only). For a new credential nothing
              // is stored yet, so it renders as a masked "replace" input.
              <SecretField
                label={app.t("secret (write-only, sealed)", "秘密(只写,密封)")}
                hasStored={false}
                onChange={(intent) => setForm({ ...form, secret: intent.value ?? "" })}
              />
            ) : (
              <div className="banner info">
                <span>ⓘ</span>
                <span>
                  {app.t(
                    "Only a non-secret source binding is saved. Install the secret on a worker using that source ID; worker heartbeats report availability.",
                    "这里只保存无秘密的来源绑定。请用该来源 ID 在 Worker 本地安装秘密；可用性由 Worker 心跳上报。",
                  )}
                </span>
              </div>
            )}
            {enter.error instanceof Error && <div className="err">{enter.error.message}</div>}
            <div className="row" style={{ justifyContent: "flex-end" }}>
              <Button variant="primary" disabled={enter.isPending} onClick={() => enter.mutate()}>
                {form.kind === "vault" ? app.t("Seal & save", "密封保存") : app.t("Save binding", "保存绑定")}
              </Button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
