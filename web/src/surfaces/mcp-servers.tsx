// Workspace · MCP catalog: an OPTIONAL reusable-definition layer. The default
// (design/web-ui.md §1, option B) is inline MCP on the agent — {type,name,url}
// with no auth (byte-identical to the Anthropic Managed Agents wire; our
// McpServerWire already matches). The runtime credential comes from the project
// Vault by url match. `credential_binding` lives ONLY on this config object
// (never on the wire), so it stays — deprecated + optional — as the central
// governance path; it does not affect SDK compatibility (§7.9c).

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api/client";
import type { CredentialBinding, McpServerDef } from "../lib/api/types";
import { useApp } from "../lib/app-state";

export default function McpServersSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const servers = useQuery({
    queryKey: ["mcp-servers"],
    queryFn: () => api.get<McpServerDef[]>("/v1/config/mcp-servers"),
  });
  const [form, setForm] = useState({ id: "", name: "", url: "", bindKind: "none", bindId: "" });
  const upsert = useMutation({
    mutationFn: () => {
      const binding: CredentialBinding =
        form.bindKind === "exact"
          ? { type: "exact", credential_source_id: form.bindId }
          : form.bindKind === "one_of_credential_pool"
            ? { type: "one_of_credential_pool", credential_pool_id: form.bindId }
            : { type: "none" };
      return api.put<McpServerDef>(`/v1/config/mcp-servers/${form.id}`, {
        id: form.id,
        display_name: form.name || form.id,
        url: form.url,
        credential_binding: binding,
        version: 1,
      });
    },
    onSuccess: () => {
      setForm({ id: "", name: "", url: "", bindKind: "none", bindId: "" });
      void qc.invalidateQueries({ queryKey: ["mcp-servers"] });
    },
  });
  return (
    <>
      <div className="banner info">
        <span>ⓘ</span>
        <span>
          {app.t(
            "Optional reusable catalog. The default is inline MCP on the agent (url only); credentials come from the project Vault by url match. Use this only for central reuse/governance.",
            "可选复用目录。默认在 agent 上内联 MCP(仅 url);凭证由 Project Vault 按 url 匹配提供。仅在需要集中复用/治理时使用。",
          )}
        </span>
      </div>
      <div className="card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>ID</th>
              <th>{app.t("Name", "名称")}</th>
              <th>URL</th>
              <th>{app.t("Credential binding", "凭证绑定")}</th>
              <th>v</th>
            </tr>
          </thead>
          <tbody>
            {(servers.data ?? []).map((s) => (
              <tr key={s.id}>
                <td className="mono">{s.id}</td>
                <td>{s.display_name}</td>
                <td className="mono mut">{s.url}</td>
                <td>
                  <span className="pill neutral">
                    {s.credential_binding.type}
                    {"credential_source_id" in s.credential_binding && ` · ${s.credential_binding.credential_source_id}`}
                    {"credential_pool_id" in s.credential_binding && ` · ${s.credential_binding.credential_pool_id}`}
                  </span>
                </td>
                <td className="mut">{s.version}</td>
              </tr>
            ))}
            {(servers.data ?? []).length === 0 && (
              <tr>
                <td colSpan={5} className="mut">
                  {app.t("No MCP servers authored yet.", "尚未作者化 MCP 服务器。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <div className="card">
        <h2>{app.t("Author MCP server", "作者化 MCP 服务器")}</h2>
        <p className="hint">
          {app.t(
            "A catalog entry is {type,name,url} — the shape agents inline directly. The credential binding below is OPTIONAL and deprecated: leave it none and let the project Vault supply the credential by url. It stays only for the central-governance path (validated fail-closed on write).",
            "目录项就是 {type,name,url}——agent 直接内联的形状。下方的凭证绑定是可选、弃用项:留 none,让 Project Vault 按 url 提供凭证。它仅为集中治理路径保留(写入时 fail-closed 校验)。",
          )}
        </p>
        <div className="row">
          <span className="field">
            <label>id</label>
            <input className="input mono" value={form.id} onChange={(e) => setForm({ ...form, id: e.target.value })} />
          </span>
          <span className="field">
            <label>{app.t("display name", "显示名")}</label>
            <input className="input" value={form.name} onChange={(e) => setForm({ ...form, name: e.target.value })} />
          </span>
          <span className="field" style={{ flex: 1 }}>
            <label>url</label>
            <input className="input mono" placeholder="https://" value={form.url} onChange={(e) => setForm({ ...form, url: e.target.value })} />
          </span>
          <span className="field">
            <label>{app.t("binding (optional · deprecated)", "绑定(可选·弃用)")}</label>
            <select className="input" value={form.bindKind} onChange={(e) => setForm({ ...form, bindKind: e.target.value })}>
              <option value="none">none</option>
              <option value="exact">exact</option>
              <option value="one_of_credential_pool">pool</option>
            </select>
          </span>
          {form.bindKind !== "none" && (
            <span className="field">
              <label>{form.bindKind === "exact" ? "credential_source_id" : "credential_pool_id"}</label>
              <input className="input mono" value={form.bindId} onChange={(e) => setForm({ ...form, bindId: e.target.value })} />
            </span>
          )}
          <button className="btn primary" style={{ alignSelf: "flex-end" }} disabled={!form.id || !form.url || upsert.isPending} onClick={() => upsert.mutate()}>
            {app.t("Save", "保存")}
          </button>
        </div>
        {upsert.error instanceof Error && <div className="err">{upsert.error.message}</div>}
      </div>
      <div className="banner gate">
        <span>◌</span>
        <span>{app.t("Health/status + Restart land with the probe extension (roadmap §7.9).", "健康状态与 Restart 随探针扩展落地(路线 §7.9)。")}</span>
      </div>
    </>
  );
}
