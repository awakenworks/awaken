// Workspace · Access: embedded-IAM token management. The mint response is the
// ONLY time the cleartext is visible — surfaced once with a copy affordance.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api, isAbsent } from "../lib/api/client";
import type { IamTokenView } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import GatedPage from "../components/app/GatedPage";
import { Button, Card, Pill, TextField, SelectField } from "../components/ui";

const WORKSPACE = "wrkspc_default";

export default function AccessSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const tokens = useQuery({
    queryKey: ["iam-tokens"],
    queryFn: () => api.get<IamTokenView[]>(`/v1/config/iam/tokens?workspace_id=${WORKSPACE}`),
    retry: false,
  });
  const [form, setForm] = useState({ name: "console", role: "workspace_admin" });
  const [minted, setMinted] = useState<string | null>(null);
  const mint = useMutation({
    mutationFn: () =>
      api.post<Record<string, unknown>>("/v1/config/iam/tokens", {
        workspace_id: WORKSPACE,
        name: form.name,
        role: form.role,
      }),
    onSuccess: (r) => {
      const secret = ["token", "secret", "cleartext", "value"]
        .map((k) => r[k])
        .find((v): v is string => typeof v === "string");
      setMinted(secret ?? JSON.stringify(r));
      void qc.invalidateQueries({ queryKey: ["iam-tokens"] });
    },
  });
  const revoke = useMutation({
    mutationFn: (id: string) => api.del(`/v1/config/iam/tokens/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["iam-tokens"] }),
  });

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
          <Button onClick={() => navigator.clipboard.writeText(minted)}>
            copy
          </Button>
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
                  <Button variant="danger" style={{ height: 26 }} onClick={() => revoke.mutate(t.id)}>
                    {app.t("Revoke", "吊销")}
                  </Button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </Card>
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
