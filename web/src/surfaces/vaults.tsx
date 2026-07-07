// Project · Vaults: the SINGLE home for runtime/tool credentials (design/web-ui.md
// §1, option B) — MCP OAuth (auto-refreshed), static bearer, and env-var secrets,
// self-managed on the managed wire and injected at egress (the sandbox never sees
// them). Consumed via `vault_ids` at session create, matched to MCP servers by url.
// No list endpoint exists (host-ephemeral), so this keeps a local registry of
// vaults created in this browser; the routes are the bare /v1/vaults face until
// the project ingress mounts it (§7.10).

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useParams } from "react-router";
import { api } from "../lib/api/client";
import type { Vault, VaultCredential } from "../lib/api/types";
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

function vaultRegistry(pid: string): string[] {
  try {
    return JSON.parse(localStorage.getItem(`awaken.console.vaults.${pid}`) ?? "[]") as string[];
  } catch {
    return [];
  }
}
function rememberVault(pid: string, id: string) {
  const list = vaultRegistry(pid).filter((v) => v !== id);
  list.unshift(id);
  localStorage.setItem(`awaken.console.vaults.${pid}`, JSON.stringify(list.slice(0, 50)));
}

function CredentialRow({ vaultId, cred }: { vaultId: string; cred: VaultCredential }) {
  const app = useApp();
  const validate = useMutation({
    mutationFn: () =>
      api.post<{ status?: string }>(`/v1/vaults/${vaultId}/credentials/${cred.id}/mcp_oauth_validate`),
  });
  // The credential kind + fields live under `auth` (secret-free projection).
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
            <button className="btn ghost" disabled={validate.isPending} onClick={() => validate.mutate()}>
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

function VaultCard({ pid, id }: { pid: string; id: string }) {
  const app = useApp();
  const qc = useQueryClient();
  const vault = useQuery({
    queryKey: ["vault", id],
    queryFn: () => api.get<Vault & { credentials?: VaultCredential[] }>(`/v1/vaults/${id}`),
    retry: false,
  });
  const [adding, setAdding] = useState(false);
  const [type, setType] = useState("static_bearer");
  const [body, setBody] = useState(CRED_TEMPLATES.static_bearer);
  // No list endpoint exists (§7): show credentials created this session from the
  // POST response (secret-free `auth` projection), so the operator sees what landed.
  const [created, setCreated] = useState<VaultCredential[]>([]);
  const create = useMutation({
    mutationFn: (payload: unknown) => api.post<VaultCredential>(`/v1/vaults/${id}/credentials`, payload),
    onSuccess: (cred) => {
      setAdding(false);
      setCreated((prev) => [cred, ...prev.filter((c) => c.id !== cred.id)]);
      void qc.invalidateQueries({ queryKey: ["vault", id] });
    },
  });
  const del = useMutation({
    mutationFn: () => api.del(`/v1/vaults/${id}`),
    onSuccess: () => {
      localStorage.setItem(
        `awaken.console.vaults.${pid}`,
        JSON.stringify(vaultRegistry(pid).filter((v) => v !== id)),
      );
      void qc.invalidateQueries();
    },
  });
  const creds = created;
  return (
    <div className="card" style={{ padding: 0 }}>
      <div className="row" style={{ padding: "12px 16px" }}>
        <code>{id}</code>
        {vault.isError && <span className="pill warn">{app.t("not found (ephemeral?)", "不存在(进程重建?)")}</span>}
        <span style={{ flex: 1 }} />
        <button className="btn ghost" onClick={() => setAdding(true)}>
          + {app.t("Add credential", "添加凭证")}
        </button>
        <button className="btn danger" onClick={() => del.mutate()}>
          {app.t("Delete", "删除")}
        </button>
      </div>
      {creds.length > 0 && (
        <>
          <div className="mut" style={{ padding: "0 16px 4px", fontSize: 11 }}>
            {app.t("Created this session (no list endpoint yet — §7).", "本次会话内新建(暂无列表端点——§7)。")}
          </div>
          <table className="table">
            <tbody>
              {creds.map((c) => (
                <CredentialRow key={c.id} vaultId={id} cred={c} />
              ))}
            </tbody>
          </table>
        </>
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
              <textarea className="input mono" rows={7} value={body} onChange={(e) => setBody(e.target.value)} />
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
  const { pid = "" } = useParams();
  const qc = useQueryClient();
  const [known, setKnown] = useState(() => vaultRegistry(pid));
  const [vaultName, setVaultName] = useState("runtime");
  const create = useMutation({
    mutationFn: () => api.post<Vault>("/v1/vaults", { display_name: vaultName || "vault" }),
    onSuccess: (v) => {
      rememberVault(pid, v.id);
      setKnown(vaultRegistry(pid));
      void qc.invalidateQueries();
    },
  });
  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="banner gate" style={{ flex: 1, marginRight: 10 }}>
          <span>ⓘ</span>
          <span>
            {app.t(
              "The single home for runtime/tool credentials — MCP OAuth (auto-refreshed), static bearer, env-var — injected at egress, matched to MCP servers by url. Views are host-ephemeral; project-ingress mounting is roadmap §7.10 (bare /v1/vaults face for now).",
              "运行/工具凭证的唯一家——MCP OAuth(自动续期)、static bearer、env-var——egress 注入、按 url 匹配 MCP。视图随进程重建;挂到项目 ingress 是路线 §7.10(暂走裸 /v1/vaults 面)。",
            )}
          </span>
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
      {create.error instanceof Error && <div className="err">{create.error.message}</div>}
      {known.map((id) => (
        <VaultCard key={id} pid={pid} id={id} />
      ))}
      {known.length === 0 && <div className="mut">{app.t("No vaults known to this browser yet.", "本浏览器还没有已知的 vault。")}</div>}
    </>
  );
}
