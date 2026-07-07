// Project · Vaults: the single home for runtime/tool credentials (design/web-ui.md
// §1, option B) — MCP OAuth (auto-refreshed), static bearer, and env-var secrets,
// self-managed on the managed wire and injected at egress (the sandbox never sees
// them). Consumed via `vault_ids` at session create, matched to MCP servers by url.
// Real list endpoints: GET /v1/vaults and GET /v1/vaults/:id/credentials.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api/client";
import type { Page, Vault, VaultCredential } from "../lib/api/types";
import { useApp } from "../lib/app-state";

// Per-type create templates (flat, `type`-tagged — the shape /v1/vaults/:id/
// credentials accepts, matching the Anthropic *CreateParams). Secret fields are
// write-only. `type` is added by the form; the body is the rest.
const CRED_TEMPLATES: Record<string, string> = {
  static_bearer: `{
  "mcp_server_url": "https://mcp.example.com/docs",
  "token": "…"
}`,
  environment_variable: `{
  "secret_name": "MY_API_KEY",
  "secret_value": "…",
  "networking": { "type": "limited", "allowed_hosts": ["api.example.com"] }
}`,
  mcp_oauth: `{
  "mcp_server_url": "https://mcp.example.com/mcp",
  "access_token": "…",
  "refresh": {
    "client_id": "…",
    "refresh_token": "…",
    "token_endpoint": "https://provider.example.com/oauth/token",
    "token_endpoint_auth": { "type": "none" }
  }
}`,
};

function CredentialRow({ vaultId, cred }: { vaultId: string; cred: VaultCredential }) {
  const app = useApp();
  const validate = useMutation({
    mutationFn: () =>
      api.post<{ status?: string }>(`/v1/vaults/${vaultId}/credentials/${cred.id}/mcp_oauth_validate`),
  });
  const kind = cred.auth?.type ?? "?";
  const target = cred.auth?.mcp_server_url ?? cred.auth?.secret_name ?? "—";
  return (
    <tr>
      <td className="mono">{cred.id}</td>
      <td>
        <span className="pill neutral">{kind}</span>
      </td>
      <td className="mono mut">{target}</td>
      <td style={{ textAlign: "right" }}>
        {kind === "mcp_oauth" ? (
          <span className="row" style={{ justifyContent: "flex-end" }}>
            {validate.data && (
              <span className={`pill ${validate.data.status === "valid" ? "ok" : "warn"}`}>
                {validate.data.status ?? "checked"}
              </span>
            )}
            <button className="btn ghost" style={{ height: 22 }} disabled={validate.isPending} onClick={() => validate.mutate()}>
              {app.t("Validate", "验证")}
            </button>
          </span>
        ) : (
          <span className="mut">—</span>
        )}
      </td>
    </tr>
  );
}

function VaultCard({ id, name }: { id: string; name?: string }) {
  const app = useApp();
  const qc = useQueryClient();
  const creds = useQuery({
    queryKey: ["vault-credentials", id],
    queryFn: () => api.get<Page<VaultCredential>>(`/v1/vaults/${id}/credentials`),
  });
  const [adding, setAdding] = useState(false);
  const [type, setType] = useState("static_bearer");
  const [body, setBody] = useState(CRED_TEMPLATES.static_bearer);
  const create = useMutation({
    mutationFn: (payload: unknown) => api.post(`/v1/vaults/${id}/credentials`, payload),
    onSuccess: () => {
      setAdding(false);
      void qc.invalidateQueries({ queryKey: ["vault-credentials", id] });
    },
  });
  const del = useMutation({
    mutationFn: () => api.del(`/v1/vaults/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["vaults"] }),
  });
  const rows = creds.data?.data ?? [];
  return (
    <div className="card" style={{ padding: 0 }}>
      <div className="row" style={{ padding: "12px 16px" }}>
        <code>{id}</code>
        {name && <span className="mut">{name}</span>}
        <span style={{ flex: 1 }} />
        <button className="btn ghost" onClick={() => setAdding(true)}>
          + {app.t("Add credential", "添加凭证")}
        </button>
        <button className="btn danger" onClick={() => del.mutate()}>
          {app.t("Delete", "删除")}
        </button>
      </div>
      {rows.length > 0 && (
        <table className="table">
          <tbody>
            {rows.map((c) => (
              <CredentialRow key={c.id} vaultId={id} cred={c} />
            ))}
          </tbody>
        </table>
      )}
      {rows.length === 0 && (
        <div className="mut" style={{ padding: "0 16px 12px", fontSize: 12 }}>
          {creds.isLoading ? "…" : app.t("No credentials yet.", "还没有凭证。")}
        </div>
      )}
      {adding && (
        <div className="overlay" onClick={() => setAdding(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <h3>{app.t("Add credential", "添加凭证")}</h3>
            <div className="row">
              {["environment_variable", "static_bearer", "mcp_oauth"].map((t) => (
                <button
                  key={t}
                  className={`btn ${type === t ? "primary" : "ghost"}`}
                  onClick={() => {
                    setType(t);
                    setBody(CRED_TEMPLATES[t] ?? body);
                  }}
                >
                  {t}
                </button>
              ))}
            </div>
            <div className="field">
              <label>{app.t("Payload (secret fields are write-only)", "请求体(秘密字段只写)")}</label>
              <textarea className="input mono" rows={8} value={body} onChange={(e) => setBody(e.target.value)} />
            </div>
            {create.error instanceof Error && <div className="err">{create.error.message}</div>}
            <div className="row" style={{ justifyContent: "flex-end" }}>
              <button
                className="btn primary"
                onClick={() => {
                  try {
                    create.mutate({ type, ...(JSON.parse(body) as object) });
                  } catch {
                    create.mutate({ type });
                  }
                }}
              >
                {app.t("Save", "保存")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

export default function VaultsSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const [vaultName, setVaultName] = useState("runtime");
  const vaults = useQuery({
    queryKey: ["vaults"],
    queryFn: () => api.get<Page<Vault>>("/v1/vaults"),
    refetchInterval: 30_000,
  });
  const create = useMutation({
    mutationFn: () => api.post<Vault>("/v1/vaults", { display_name: vaultName || "vault" }),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["vaults"] }),
  });
  const rows = (vaults.data?.data ?? []).filter((v) => !v.archived_at);
  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut" style={{ flex: 1, marginRight: 10 }}>
          {app.t(
            "The single home for runtime/tool credentials — MCP OAuth (auto-refreshed), static bearer, env-var — injected at egress, matched to MCP servers by url.",
            "运行/工具凭证的唯一家——MCP OAuth(自动续期)、static bearer、env-var——egress 注入、按 url 匹配 MCP。",
          )}
        </span>
        <span className="row">
          <input
            className="input"
            style={{ width: 140 }}
            value={vaultName}
            onChange={(e) => setVaultName(e.target.value)}
            placeholder={app.t("display name", "显示名")}
          />
          <button className="btn primary" disabled={create.isPending} onClick={() => create.mutate()}>
            + {app.t("Create vault", "创建 vault")}
          </button>
        </span>
      </div>
      {(create.error instanceof Error || vaults.error instanceof Error) && (
        <div className="err">{(create.error || vaults.error)?.message}</div>
      )}
      {rows.map((v) => (
        <VaultCard key={v.id} id={v.id} name={v.display_name} />
      ))}
      {rows.length === 0 && (
        <div className="mut">{vaults.isLoading ? "…" : app.t("No vaults yet.", "还没有 vault。")}</div>
      )}
    </>
  );
}
