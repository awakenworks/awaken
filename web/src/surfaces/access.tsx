// Workspace · Access: embedded-IAM token management. The mint response is the
// ONLY time the cleartext is visible — surfaced once with a copy affordance.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api, isAbsent, workspaceFields, workspaceQuery, ws } from "../lib/api/client";
import type { IamTokenView } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import GatedPage from "../components/app/GatedPage";
import { Button, Card, CopyButton, Pill, TextField, SelectField, useConfirm, useToast } from "../components/ui";

function roleLabel(role: string | undefined, t: (en: string, zh: string) => string) {
  switch (role) {
    case "workspace_restricted_developer": return t("Read-only developer", "只读开发者");
    case "workspace_admin": return t("Workspace administrator", "工作区管理员");
    case "admin": return t("Platform administrator", "平台管理员");
    default: return role ?? t("Unknown role", "未知角色");
  }
}

export default function AccessSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
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
  const [minted, setMinted] = useState<string | null>(null);
  const mint = useMutation({
    mutationFn: () =>
      api.post<Record<string, unknown>>(ws("/v1/config/iam/tokens"), {
        ...workspaceFields(workspace),
        principal_id: form.principalId.trim(),
        role: form.role,
        ...(form.expiresAt ? { expires_at: form.expiresAt } : {}),
      }),
    onSuccess: (r) => {
      const secret = ["token", "secret", "cleartext", "value"]
        .map((k) => r[k])
        .find((v): v is string => typeof v === "string");
      setMinted(secret ?? JSON.stringify(r));
      void qc.invalidateQueries({ queryKey: ["iam-tokens", workspace] });
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const revoke = useMutation({
    mutationFn: (id: string) => api.del(ws(`/v1/config/iam/tokens/${id}`)),
    onSuccess: () => {
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
        endpoint="/v1/config/iam/tokens (AWAKEN_MGMT_IAM=embedded)"
        note={app.t("start the host with embedded IAM to manage tokens", "以嵌入式 IAM 启动 host 才能管理令牌")}
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
            <code>{minted}</code>
          </span>
          <CopyButton value={minted} label={app.t("Copy", "复制")} copiedLabel={app.t("Copied", "已复制")} />
          <Button variant="ghost" onClick={() => setMinted(null)}>
            ✕
          </Button>
        </div>
      )}
      <div className="banner info">
        <span>🔐</span>
        <span>{app.t("Create long-lived, workspace-scoped service API keys here for trusted backends. Browser/mobile clients must use short-lived application access tokens from API & protocols.", "在这里为可信后端创建长期、限定 Workspace 的 Service API Key；浏览器和移动端必须使用“API 与协议”中的短期 Application Access Token。")}</span>
      </div>
      <Card style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>ID</th>
              <th>{app.t("Service principal", "服务主体")}</th>
              <th>{app.t("Role", "角色")}</th>
              <th>{app.t("Lifecycle", "生命周期")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(tokens.data ?? []).map((t) => (
              <tr key={t.id}>
                <td className="mono">{t.id}</td>
                <td><strong>{t.principal_id ?? "—"}</strong><div className="mono mut">{t.prefix ?? t.id}</div></td>
                <td>
                  <Pill tone="neutral">{roleLabel(t.role, app.t)}</Pill>
                </td>
                <td>{t.revoked_at ? <Pill tone="neutral">{app.t("revoked", "已吊销")}</Pill> : <span className="mut">{t.expires_at ? `${app.t("expires", "到期")} ${t.expires_at}` : app.t("no expiry", "永不过期")}</span>}</td>
                <td style={{ textAlign: "right" }}>
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
        {mint.error instanceof Error && <div className="err">{mint.error.message}</div>}
        <p className="mut" style={{ marginBottom: 0 }}>
          {app.t(
            "Use read-only developer by default. Choose workspace or platform administrator only when the service must change configuration.",
            "默认使用只读开发者；只有服务确实需要修改配置时，才选择工作区或平台管理员。",
          )}
        </p>
      </Card>
    </>
  );
}
