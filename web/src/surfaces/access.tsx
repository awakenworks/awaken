// Workspace · Access: embedded-IAM token management. The mint response is the
// ONLY time the cleartext is visible — surfaced once with a copy affordance.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api, isAbsent, workspaceFields, workspaceQuery, ws } from "../lib/api/client";
import type { IamTokenView } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import GatedPage from "../components/app/GatedPage";
import { Button, Card, CopyButton, Pill, TextField, SelectField, useConfirm, useToast } from "../components/ui";

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
  const [form, setForm] = useState({ name: "console", role: "workspace_admin" });
  const [minted, setMinted] = useState<string | null>(null);
  const mint = useMutation({
    mutationFn: () =>
      api.post<Record<string, unknown>>(ws("/v1/config/iam/tokens"), {
        ...workspaceFields(workspace),
        name: form.name,
        role: form.role,
      }),
    onSuccess: (r) => {
      const secret = ["token", "secret", "cleartext", "value"]
        .map((k) => r[k])
        .find((v): v is string => typeof v === "string");
      setMinted(secret ?? JSON.stringify(r));
      void qc.invalidateQueries({ queryKey: ["iam-tokens", workspace] });
    },
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
          <CopyButton value={minted} />
          <Button variant="ghost" onClick={() => setMinted(null)}>
            ✕
          </Button>
        </div>
      )}
      <Card style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>ID</th>
              <th>{app.t("Name", "名称")}</th>
              <th>{app.t("Role", "角色")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {(tokens.data ?? []).map((t) => (
              <tr key={t.id}>
                <td className="mono">{t.id}</td>
                <td>{t.name ?? "—"}</td>
                <td>
                  <Pill tone="neutral">{t.role ?? "?"}</Pill>
                </td>
                <td style={{ textAlign: "right" }}>
                  <Button variant="danger" style={{ height: 26 }} disabled={revoke.isPending && revoke.variables === t.id} onClick={() => void revokeToken(t.id)}>
                    {revoke.isPending && revoke.variables === t.id ? app.t("Revoking…", "正在吊销…") : app.t("Revoke", "吊销")}
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </Card>
      {tokens.error instanceof Error && <div className="err">{tokens.error.message}</div>}
      <Card>
        <h2>{app.t("Mint token", "铸造令牌")}</h2>
        <div className="row">
          <TextField label={app.t("Name", "名称")} value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} />
          <SelectField label={app.t("Role", "角色")} value={form.role} onChange={(e) => setForm({ ...form, role: e.target.value })}>
            <option>admin</option>
            <option>workspace_admin</option>
            <option>workspace_restricted_developer</option>
            <option>workspace_user</option>
          </SelectField>
          <Button variant="primary" style={{ alignSelf: "flex-end" }} disabled={mint.isPending} onClick={() => mint.mutate()}>
            {app.t("Mint", "铸造")}
          </Button>
        </div>
        {mint.error instanceof Error && <div className="err">{mint.error.message}</div>}
        <p className="mut" style={{ marginBottom: 0 }}>
          {app.t(
            "Roles: admin / workspace_admin / workspace_restricted_developer (read-all) / workspace_user (no apikey).",
            "角色:admin / workspace_admin / restricted_developer(全读)/ workspace_user(无 apikey 权限)。",
          )}
        </p>
      </Card>
    </>
  );
}
