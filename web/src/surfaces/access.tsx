// Workspace · Access: embedded-IAM token management. The mint response is the
// ONLY time the cleartext is visible — surfaced once with a copy affordance.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useParams } from "react-router";
import { api, isAbsent, workspaceFields, workspaceQuery, ws } from "../lib/api/client";
import type { IamTokenMintResponse, IamTokenView } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { protocolHelpPath } from "../lib/navigation/paths";
import { entityDisplayName, identifierLabel } from "../lib/presentation";
import GatedPage from "../components/app/GatedPage";
import { Button, Card, CopyButton, Pill, TechnicalId, TextField, SelectField, useConfirm, useToast } from "../components/ui";

function roleLabel(role: string | undefined, t: (en: string, zh: string) => string) {
  switch (role) {
    case "workspace_restricted_developer": return t("Read-only developer", "只读开发者");
    case "workspace_admin": return t("Workspace administrator", "工作区管理员");
    case "admin": return t("Platform administrator", "平台管理员");
    default: return role ?? t("Unknown role", "未知角色");
  }
}

export function serviceKeyRoleGuidance(role: string, locale: "en" | "zh") {
  const zh = locale === "zh";
  switch (role) {
    case "workspace_restricted_developer":
      return zh ? "可读取工作区，但不能通过 SDK 创建 Session。" : "Can inspect the Workspace, but cannot create Sessions through the SDK.";
    case "workspace_admin":
      return zh ? "可创建 Session，也可修改工作区配置；SDK 快速开始当前需要此角色。" : "Can create Sessions and change Workspace configuration; the SDK quickstart currently requires this role.";
    case "admin":
      return zh ? "可管理整个平台；仅用于确实需要跨工作区管理的服务。" : "Can administer the platform; use only for services that truly need cross-Workspace control.";
    default:
      return zh ? "请按服务实际工作选择最小权限角色。" : "Choose the least-privileged role that can complete the service's work.";
  }
}

export function mintedServiceKey(response: IamTokenMintResponse) {
  const secret = [response.token, response.secret, response.cleartext, response.value]
    .find((value): value is string => typeof value === "string" && value.length > 0);
  if (!secret) throw new Error("The server did not return a one-time service API key.");
  return { id: response.api_token?.id, secret };
}

export default function AccessSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
  const { ws: wsId = "default" } = useParams();
  const qc = useQueryClient();
  const confirm = useConfirm();
  const toast = useToast();
  const tokens = useQuery({
    queryKey: ["iam-tokens", workspace],
    queryFn: () => api.get<IamTokenView[]>(ws(workspaceQuery("/v1/config/iam/tokens", workspace))),
    retry: false,
  });
  const [form, setForm] = useState({
    principalId: "service-console",
    role: "workspace_restricted_developer",
    expiresAt: "",
  });
  const [minted, setMinted] = useState<{ id?: string; secret: string } | null>(null);
  const mint = useMutation({
    mutationFn: async () => {
      const response = await api.post<IamTokenMintResponse>(ws("/v1/config/iam/tokens"), {
        ...workspaceFields(workspace),
        principal_id: form.principalId.trim(),
        role: form.role,
        ...(form.expiresAt ? { expires_at: form.expiresAt } : {}),
      });
      return mintedServiceKey(response);
    },
    onSuccess: (r) => {
      setMinted(r);
      void qc.invalidateQueries({ queryKey: ["iam-tokens", workspace] });
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const revoke = useMutation({
    mutationFn: (id: string) => api.del(ws(`/v1/config/iam/tokens/${id}`)),
    onSuccess: (_, id) => {
      setMinted((current) => current?.id === id ? null : current);
      void qc.invalidateQueries({ queryKey: ["iam-tokens", workspace] });
      toast.ok(app.t("Token revoked.", "令牌已吊销。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const revokeToken = async (id: string) => {
    const approved = await confirm({
      title: app.t("Revoke this token?", "吊销该令牌？"),
      body: app.t("Clients using it will lose access immediately. This cannot be undone.", "使用它的客户端会立即失去访问权限，且无法撤销。"),
      confirmLabel: app.t("Revoke", "吊销"),
      danger: true,
    });
    if (approved) revoke.mutate(id);
  };

  if (tokens.isError && isAbsent(tokens.error)) {
    return (
      <GatedPage
        title="Access"
        endpoint="/v1/config/iam/tokens (identity_mode=self-managed)"
        note={app.t("start all-in-one with self-managed identity to manage service API keys", "以 self-managed 身份模式启动 all-in-one，才能管理 Service API Key")}
      />
    );
  }
  return (
    <>
      {minted && (
        <div className="banner warn">
          <span>🔑</span>
          <span style={{ flex: 1 }}>
            {app.t("Cleartext shown ONCE — copy it now: ", "明文只显示一次——立即复制:")}
            <code>{minted.secret}</code>
          </span>
          <CopyButton value={minted.secret} label={app.t("Copy", "复制")} copiedLabel={app.t("Copied", "已复制")} />
          <Link className="protocol-help-link" to={protocolHelpPath(wsId, "managed")}>
            {app.t("Open SDK setup →", "打开 SDK 配置 →")}
          </Link>
          <Button variant="ghost" onClick={() => setMinted(null)}>
            ✕
          </Button>
        </div>
      )}
      <div className="banner info">
        <span>🔐</span>
        <span>{app.t("Create long-lived, workspace-scoped service API keys here for trusted backends. Browser/mobile clients must use short-lived application access tokens from API & protocols.", "在这里为可信后端创建长期、限定 Workspace 的 Service API Key；浏览器和移动端必须使用“API 与协议”中的短期 Application Access Token。")}</span>
      </div>
      <Card>
        <h2>{app.t("Connect a trusted backend", "连接可信后端")}</h2>
        <ol className="protocol-endpoints">
          <li>{app.t("Create a dedicated service principal. For the Managed Agents SDK quickstart, choose Workspace administrator so the client can create a Session.", "创建专用服务主体。Managed Agents SDK 快速开始需要创建 Session，因此请选择“工作区管理员”。")}</li>
          <li>{app.t("Set an expiry, create the key, and copy the secret shown once.", "设置到期时间，创建 Key，并复制仅显示一次的明文。")}</li>
          <li>{app.t("Open the Managed Agents guide, run its SDK example from a trusted backend, then confirm the same Session in Console.", "打开 Managed Agents 指南，从可信后端运行 SDK 示例，再在 Console 中确认同一个 Session。")}</li>
        </ol>
        <p className="hint">
          {app.t("Application access tokens are for browser and mobile protocols. They do not replace this service key for a trusted Managed Agents backend.", "Application Access Token 用于浏览器和移动端协议，不能替代可信 Managed Agents 后端使用的 Service API Key。")}
        </p>
        <Link className="protocol-help-link" to={protocolHelpPath(wsId, "managed")}>
          {app.t("Open Managed Agents SDK setup →", "打开 Managed Agents SDK 配置 →")}
        </Link>
      </Card>
      <Card className="responsive-table-card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Service principal", "服务主体")}</th>
              <th>{app.t("Role", "角色")}</th>
              <th>{app.t("Lifecycle", "生命周期")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(tokens.data ?? []).map((t) => (
              <tr key={t.id}>
                <td data-label={app.t("Service principal", "服务主体")}>
                  <strong>{entityDisplayName(t.principal_id ? identifierLabel(t.principal_id) : "", app.t("Service account", "服务账户"))}</strong>
                  <TechnicalId value={`${t.principal_id ?? "unknown-principal"} · ${t.id}`} />
                </td>
                <td data-label={app.t("Role", "角色")}>
                  <Pill tone="neutral">{roleLabel(t.role, app.t)}</Pill>
                </td>
                <td data-label={app.t("Lifecycle", "生命周期")}>{t.revoked_at ? <Pill tone="neutral">{app.t("revoked", "已吊销")}</Pill> : <span className="mut">{t.expires_at ? `${app.t("expires", "到期")} ${t.expires_at}` : app.t("no expiry", "永不过期")}</span>}</td>
                <td className="responsive-table-actions" style={{ textAlign: "right" }}>
                  <Button variant="danger" style={{ height: 26 }} disabled={!!t.revoked_at || (revoke.isPending && revoke.variables === t.id)} onClick={() => void revokeToken(t.id)}>
                    {t.revoked_at
                      ? app.t("Revoked", "已吊销")
                      : revoke.isPending && revoke.variables === t.id
                        ? app.t("Revoking…", "正在吊销…")
                        : app.t("Revoke", "吊销")}
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </Card>
      {tokens.error instanceof Error && <div className="err">{tokens.error.message}</div>}
      <Card>
        <h2>{app.t("Create service API key", "创建 Service API Key")}</h2>
        <div className="row">
          <TextField label={app.t("Service principal", "服务主体")} value={form.principalId} onChange={(e) => setForm({ ...form, principalId: e.target.value })} placeholder="billing-backend" />
          <SelectField label={app.t("Role", "角色")} value={form.role} onChange={(e) => setForm({ ...form, role: e.target.value })}>
            <option value="workspace_restricted_developer">{app.t("Read-only developer", "只读开发者")}</option>
            <option value="workspace_admin">{app.t("Workspace administrator", "工作区管理员")}</option>
            <option value="admin">{app.t("Platform administrator", "平台管理员")}</option>
          </SelectField>
          <TextField label={app.t("Expires at · optional", "到期时间 · 可选")} mono value={form.expiresAt} onChange={(e) => setForm({ ...form, expiresAt: e.target.value })} placeholder="2027-01-01T00:00:00Z" />
          <Button variant="primary" style={{ alignSelf: "flex-end" }} disabled={mint.isPending || !form.principalId.trim()} onClick={() => mint.mutate()}>
            {app.t("Create key", "创建 Key")}
          </Button>
        </div>
        <p className="hint">{serviceKeyRoleGuidance(form.role, app.locale)}</p>
        {mint.error instanceof Error && <div className="err">{mint.error.message}</div>}
        <p className="mut" style={{ marginBottom: 0 }}>
          {app.t(
            "Use read-only developer for inspection. The current self-managed policy uses Workspace administrator for clients that create Sessions; keep the principal dedicated, set an expiry, and revoke it when no longer needed.",
            "只读检查使用“只读开发者”。当前 self-managed 策略下，创建 Session 的客户端需使用“工作区管理员”；请使用专用主体、设置到期时间，并在不再需要时吊销。",
          )}
        </p>
      </Card>
    </>
  );
}
