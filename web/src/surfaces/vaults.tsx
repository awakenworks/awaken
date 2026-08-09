// Workspace · Runtime secrets: tool/integration credentials only. Inference
// credentials have one separate authority in the model supply domain.
// MCP OAuth (auto-refreshed), static bearer, and env-var secrets,
// self-managed on the managed wire and injected at egress (the sandbox never sees
// them). Consumed via `vault_ids` at session create, matched to MCP servers by url.
// Real list endpoints: GET /v1/vaults and GET /v1/vaults/:id/credentials.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useConfirm } from "../components/ui/Confirm";
import { useToast } from "../components/ui/Toast";
import { Button, Card, Modal, Pill, SecretField, TextField } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { Page, Vault, VaultCredential } from "../lib/api/types";
import { useApp } from "../lib/app-state";

type RuntimeCredentialType = "environment_variable" | "static_bearer" | "mcp_oauth";

export const RUNTIME_CREDENTIAL_TYPES: ReadonlyArray<{ type: RuntimeCredentialType; label: string; labelZh: string; consumer: string; consumerZh: string }> = [
  { type: "environment_variable", label: "Environment variable", labelZh: "环境变量", consumer: "Packages and command-line tools", consumerZh: "供 Package 和命令行工具使用" },
  { type: "static_bearer", label: "MCP bearer token", labelZh: "MCP Bearer Token", consumer: "One matching HTTP MCP server", consumerZh: "供一个匹配的 HTTP MCP Server 使用" },
  { type: "mcp_oauth", label: "MCP OAuth", labelZh: "MCP OAuth", consumer: "One MCP server with optional renewal", consumerZh: "供一个 MCP Server 使用，可自动续期" },
];

function credentialTypesForVault(name?: string) {
  if (name?.startsWith("Integrations ·")) {
    return RUNTIME_CREDENTIAL_TYPES.filter((option) => option.type !== "environment_variable");
  }
  if (name?.startsWith("Sandbox environment ·") || name?.startsWith("Shared runtime ·")) {
    return RUNTIME_CREDENTIAL_TYPES.filter((option) => option.type === "environment_variable");
  }
  return RUNTIME_CREDENTIAL_TYPES;
}

function CredentialRow({ vaultId, cred }: { vaultId: string; cred: VaultCredential }) {
  const app = useApp();
  const validate = useMutation({
    mutationFn: () =>
      api.post<{ status?: string }>(ws(`/v1/vaults/${vaultId}/credentials/${cred.id}/mcp_oauth_validate`)),
  });
  const kind = cred.auth?.type ?? "?";
  const typeInfo = RUNTIME_CREDENTIAL_TYPES.find((option) => option.type === kind);
  const target = cred.auth?.mcp_server_url ?? cred.auth?.secret_name ?? "—";
  return (
    <tr>
      <td className="mono">{cred.id}</td>
      <td>
        <Pill tone="neutral">{typeInfo ? app.t(typeInfo.label, typeInfo.labelZh) : kind}</Pill>
      </td>
      <td><strong>{cred.display_name ?? target}</strong>{cred.display_name && <div className="mono mut">{target}</div>}</td>
      <td style={{ textAlign: "right" }}>
        {kind === "mcp_oauth" ? (
          <span className="row" style={{ justifyContent: "flex-end" }}>
            {validate.data && (
              <Pill tone={validate.data.status === "valid" ? "ok" : "warn"}>
                {validate.data.status ?? "checked"}
              </Pill>
            )}
            <Button variant="ghost" style={{ height: 22 }} disabled={validate.isPending} onClick={() => validate.mutate()}>
              {app.t("Validate", "验证")}
            </Button>
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
  const confirm = useConfirm();
  const toast = useToast();
  const creds = useQuery({
    queryKey: ["vault-credentials", id],
    queryFn: () => api.get<Page<VaultCredential>>(ws(`/v1/vaults/${id}/credentials`)),
  });
  const [adding, setAdding] = useState(false);
  const [renaming, setRenaming] = useState(false);
  const [vaultName, setVaultName] = useState(name ?? "Runtime · integrations");
  const allowedTypes = credentialTypesForVault(name);
  const [type, setType] = useState<RuntimeCredentialType>(allowedTypes[0]?.type ?? "environment_variable");
  const [displayName, setDisplayName] = useState("");
  const [target, setTarget] = useState("");
  const [secret, setSecret] = useState("");
  const [allowedHosts, setAllowedHosts] = useState("");
  const [refreshEnabled, setRefreshEnabled] = useState(false);
  const [clientId, setClientId] = useState("");
  const [refreshToken, setRefreshToken] = useState("");
  const [tokenEndpoint, setTokenEndpoint] = useState("");
  const [expiresAt, setExpiresAt] = useState("");
  const create = useMutation({
    mutationFn: (payload: unknown) => api.post(ws(`/v1/vaults/${id}/credentials`), payload),
    onSuccess: () => {
      setAdding(false);
      setSecret("");
      setRefreshToken("");
      void qc.invalidateQueries({ queryKey: ["vault-credentials", id] });
    },
  });
  const rename = useMutation({
    mutationFn: () => api.post(ws(`/v1/vaults/${id}`), { display_name: vaultName.trim() }),
    onSuccess: () => {
      setRenaming(false);
      void qc.invalidateQueries({ queryKey: ["vaults"] });
    },
  });
  const del = useMutation({
    mutationFn: () => api.del(ws(`/v1/vaults/${id}`)),
    onSuccess: () => {
      toast.ok(app.t("Vault deleted.", "Vault 已删除。"));
      void qc.invalidateQueries({ queryKey: ["vaults"] });
    },
    onError: (e) => toast.err(e instanceof Error ? e.message : "error"),
  });
  const confirmDelete = async () => {
    const ok = await confirm({
      title: app.t("Delete this vault?", "删除此 Vault?"),
      body: app.t(`Vault ${id} and its credentials are permanently removed.`, `Vault ${id} 及其凭证将被永久移除。`),
      danger: true,
      confirmLabel: app.t("Delete", "删除"),
    });
    if (ok) del.mutate();
  };
  const rows = creds.data?.data ?? [];
  return (
    <Card style={{ padding: 0 }}>
      <div className="row" style={{ padding: "12px 16px" }}>
        <code>{id}</code>
        {renaming ? <span className="row"><input className="input" value={vaultName} onChange={(event) => setVaultName(event.target.value)} /><Button disabled={rename.isPending || !vaultName.trim()} onClick={() => rename.mutate()}>{app.t("Save", "保存")}</Button></span> : <button className="btn ghost" onClick={() => setRenaming(true)}>{name ?? app.t("Unnamed vault", "未命名 Vault")} · {app.t("Rename", "重命名")}</button>}
        <span style={{ flex: 1 }} />
        <Button variant="ghost" onClick={() => setAdding(true)}>
          + {app.t("Add credential", "添加凭证")}
        </Button>
        <Button variant="danger" disabled={del.isPending} onClick={confirmDelete}>
          {app.t("Delete", "删除")}
        </Button>
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
        <Modal title={app.t("Add runtime credential", "添加运行时凭证")} onClose={() => setAdding(false)} width="min(760px, 94vw)">
            <p className="hint">{app.t(
              `This Vault is categorized as ${name?.split(" · ")[0] ?? "runtime"}; only matching credential types are shown.`,
              `此 Vault 的分类是“${name?.split(" · ")[0] ?? "运行时"}”；这里只显示用途匹配的凭证类型。`,
            )}</p>
            <div className="row" style={{ alignItems: "stretch" }}>
              {allowedTypes.map((option) => (
                <Button
                  key={option.type}
                  variant={type === option.type ? "primary" : "ghost"}
                  onClick={() => {
                    setType(option.type);
                    setTarget("");
                    setSecret("");
                  }}
                >
                  <span style={{ display: "flex", flexDirection: "column", alignItems: "flex-start" }}><strong>{app.t(option.label, option.labelZh)}</strong><small>{app.t(option.consumer, option.consumerZh)}</small></span>
                </Button>
              ))}
            </div>
            <TextField label={app.t("Credential name", "凭证名称")} value={displayName} onChange={(event) => setDisplayName(event.target.value)} placeholder={type === "environment_variable" ? "GitHub · build token" : "Docs MCP · production"} />
            <TextField
              label={type === "environment_variable" ? app.t("Environment variable", "环境变量名") : "MCP server URL"}
              mono
              value={target}
              onChange={(event) => setTarget(event.target.value)}
              placeholder={type === "environment_variable" ? "GITHUB_TOKEN" : "https://mcp.example.com/mcp"}
            />
            <SecretField label={type === "mcp_oauth" ? app.t("Access token (write-only)", "Access Token（仅写入）") : app.t("Secret (write-only)", "Secret（仅写入）")} hasStored={false} onChange={(intent) => setSecret(intent.value ?? "")} />
            {type === "environment_variable" && <TextField label={app.t("Allowed hosts", "允许的 Host")} mono value={allowedHosts} onChange={(event) => setAllowedHosts(event.target.value)} placeholder="api.github.com, github.com" />}
            {type === "mcp_oauth" && <details open={refreshEnabled}><summary><label className="row" onClick={(event) => event.stopPropagation()}><input type="checkbox" checked={refreshEnabled} onChange={(event) => setRefreshEnabled(event.target.checked)} />{app.t("Refresh this token automatically", "自动刷新 Token")}</label></summary>{refreshEnabled && <div className="stack" style={{ marginTop: 10 }}><div className="grid-2"><TextField label="Client id" mono value={clientId} onChange={(event) => setClientId(event.target.value)} /><SecretField label={app.t("Refresh token", "Refresh Token")} hasStored={false} onChange={(intent) => setRefreshToken(intent.value ?? "")} /></div><TextField label="Token endpoint" mono value={tokenEndpoint} onChange={(event) => setTokenEndpoint(event.target.value)} placeholder="https://provider.example.com/oauth/token" /></div>}</details>}
            {type === "mcp_oauth" && <TextField label={app.t("Expires at · optional", "过期时间 · 可选")} mono value={expiresAt} onChange={(event) => setExpiresAt(event.target.value)} placeholder="2026-12-31T00:00:00Z" />}
            <div className="banner info"><span>ⓘ</span><span>{app.t("Runtime Secrets stores integration and execution credentials. Add model API keys under Models & providers so Awaken can verify and rotate them with the matching provider.", "Runtime Secrets 用于保存集成和执行凭证。模型 API Key 请在“模型与供应商”中添加，以便 Awaken 按对应供应商验证和轮换。")}</span></div>
            {create.error instanceof Error && <div className="err">{create.error.message}</div>}
            <div className="row" style={{ justifyContent: "flex-end" }}>
              <Button
                variant="primary"
                disabled={create.isPending || !target.trim() || !secret}
                onClick={() => {
                  if (type === "environment_variable") {
                    const hosts = allowedHosts.split(",").map((host) => host.trim()).filter(Boolean);
                    create.mutate({ type, display_name: displayName || undefined, secret_name: target, secret_value: secret, networking: hosts.length ? { type: "limited", allowed_hosts: hosts } : { type: "unrestricted" } });
                  } else if (type === "static_bearer") {
                    create.mutate({ type, display_name: displayName || undefined, mcp_server_url: target, token: secret });
                  } else {
                    create.mutate({ type, display_name: displayName || undefined, mcp_server_url: target, access_token: secret, expires_at: expiresAt || undefined, ...(refreshEnabled ? { refresh: { client_id: clientId, refresh_token: refreshToken, token_endpoint: tokenEndpoint, token_endpoint_auth: { type: "none" } } } : {}) });
                  }
                }}
              >
                {create.isPending ? app.t("Saving…", "正在保存…") : app.t("Save", "保存")}
              </Button>
            </div>
        </Modal>
      )}
    </Card>
  );
}

export default function VaultsSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const [vaultName, setVaultName] = useState("runtime");
  const [vaultCategory, setVaultCategory] = useState("Integrations");
  const vaults = useQuery({
    queryKey: ["vaults", workspace],
    queryFn: () => api.get<Page<Vault>>(ws("/v1/vaults")),
    refetchInterval: 30_000,
  });
  const create = useMutation({
    mutationFn: () => api.post<Vault>(ws("/v1/vaults"), { display_name: `${vaultCategory} · ${vaultName.trim() || "default"}` }),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["vaults", workspace] }),
  });
  const rows = (vaults.data?.data ?? []).filter((v) => !v.archived_at);
  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut" style={{ flex: 1, marginRight: 10 }}>
          {app.t(
            "Choose a category and human-readable name for each Vault. Add only credentials that share the same runtime purpose.",
            "为每个 Vault 选择分类和易读名称，并只添加用途一致的运行时凭证。",
          )}
        </span>
        <span className="row">
          <select className="input" aria-label={app.t("Vault category", "Vault 分类")} value={vaultCategory} onChange={(event) => setVaultCategory(event.target.value)}>
            <option value="Integrations">{app.t("Integrations", "集成")}</option>
            <option value="Sandbox environment">{app.t("Sandbox environment", "Sandbox 环境")}</option>
            <option value="Shared runtime">{app.t("Shared runtime", "共享运行环境")}</option>
          </select>
          <input
            className="input"
            style={{ width: 140 }}
            value={vaultName}
            onChange={(e) => setVaultName(e.target.value)}
            placeholder={app.t("Vault name", "Vault 名称")}
          />
          <Button variant="primary" disabled={create.isPending} onClick={() => create.mutate()}>
            + {app.t("Create Vault", "创建 Vault")}
          </Button>
        </span>
      </div>
      {(create.error instanceof Error || vaults.error instanceof Error) && (
        <div className="err">{(create.error || vaults.error)?.message}</div>
      )}
      {rows.map((v) => (
        <VaultCard key={v.id} id={v.id} name={v.display_name} />
      ))}
      {rows.length === 0 && (
        <div className="mut">{vaults.isLoading ? "…" : app.t("No Vaults yet. Create one for an integration, Sandbox Environment, or shared runtime purpose.", "还没有 Vault。请为集成、Sandbox 环境或共享运行用途创建一个。")}</div>
      )}
    </>
  );
}
