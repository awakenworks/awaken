import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button, Card, InlineNotice } from "@awaken/ui";
import { Link, useNavigate } from "react-router";
import { api, ws } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import type { McpConfigurationView, McpDefinition, McpInspection, McpPreparedBinding, McpServerRow } from "../../lib/api/mcp-types";
import { mcpAddress } from "../../lib/api/mcp-types";
import McpCredentialSelect from "./McpCredentialSelect";
import McpServerForm from "./McpServerForm";
import McpDebugSession from "./McpDebugSession";
import McpAgentInventory from "./McpAgentInventory";
import { TextField } from "../ui";

function ServerDetail({ row, revision, onChanged }: { row: McpServerRow; revision: number; onChanged: () => void }) {
  const app = useApp(); const nav = useNavigate();
  const [credential, setCredential] = useState(row.default_matches_target ? row.default_credential?.vault_credential_id ?? "" : "");
  const [debug, setDebug] = useState(false);
  const [inspection, setInspection] = useState<McpInspection>();
  const base = ws(`/v1/config/mcp-servers/${encodeURIComponent(row.key)}`);
  const selected = () => ({ expected_revision: revision, credential_id: credential || null });
  const check = useMutation({ mutationFn: () => api.post<McpInspection>(`${base}/inspect`, selected()), onSuccess: setInspection });
  const saveDefault = useMutation({ mutationFn: () => api.put(`${base}/default-credential`, selected()), onSuccess: onChanged });
  const prepare = () => api.post<McpPreparedBinding>(`${base}/prepare`, selected());
  return <Card className="stack">
    <div className="row">
      {row.definition.icon ? <img src={row.definition.icon} alt="" width={28} height={28} /> : <span aria-hidden="true" className="badge">{row.definition.name.slice(0, 1)}</span>}
      <strong>{row.definition.name}</strong><span className="mut">{row.origin === "local" ? app.t("Local configuration", "本地配置") : app.t("Catalog preset", "目录预设")}</span>
    </div>
    <p>{row.definition.description}</p><code style={{ overflowWrap: "anywhere" }}>{mcpAddress(row.definition)}</code>
    {!row.default_matches_target && <InlineNotice tone="warning">{app.t("The endpoint changed. Choose a matching credential again.", "目标地址已变化，请重新选择匹配凭证。")}</InlineNotice>}
    <McpCredentialSelect url={row.definition.transport.type === "url" ? row.definition.transport.url : undefined} value={credential} onChange={(choice) => { setCredential(choice?.id ?? ""); setInspection(undefined); setDebug(false); }} />
    {row.definition.transport.type === "sandbox_stdio" && <p className="hint">{app.t("Sandbox stdio runs in the selected Session environment. This configuration does not inject Vault secrets into process arguments or environment variables.", "Sandbox stdio 在所选 Session 环境中运行。此配置不会将 Vault 秘密注入进程参数或环境变量。")}</p>}
    <div className="row" style={{ flexWrap: "wrap" }}>
      <Button disabled={check.isPending || row.definition.transport.type !== "url"} onClick={() => check.mutate()}>{app.t("Check connection", "检查连接")}</Button>
      <Button disabled={saveDefault.isPending} onClick={() => saveDefault.mutate()}>{app.t("Save default credential", "保存默认凭证")}</Button>
      <Button onClick={() => setDebug(true)}>{app.t("Test with an Agent", "使用 Agent 试运行")}</Button>
      <Button onClick={() => nav(`/w/${app.workspaceId}/agents/new?mcp=${encodeURIComponent(row.key)}`)}>{app.t("Use in a new Agent", "用于新 Agent")}</Button>
    </div>
    {saveDefault.isSuccess && <span className="mut">{app.t("Default saved. Existing Agents and Sessions are unchanged.", "默认选择已保存，已有 Agent 和 Session 保持不变。")}</span>}
    {(check.error || saveDefault.error) instanceof Error && <div className="err">{(check.error || saveDefault.error)?.message}</div>}
    {inspection && <div className="stack" aria-label={app.t("Connection test result", "连接测试结果")}>
      <strong>{inspection.status} {inspection.http_status ?? ""}</strong>
      <span className="mut">{app.t("Tested from Control", "测试位置：Control")} · {inspection.checked_at} · {inspection.credential?.display_name ?? app.t("Anonymous", "匿名")}</span>
      <p className="hint">{app.t("This checks authentication, initialization and tool discovery. Session execution still verifies its own environment.", "这里检查认证、初始化和工具发现；Session 执行仍需验证其实际环境。")}</p>
      {inspection.tools.map((tool) => <details key={tool.name}><summary>{tool.name}</summary><p>{tool.description}</p><pre style={{ overflow: "auto" }}>{JSON.stringify(tool.input_schema, null, 2)}</pre></details>)}
    </div>}
    {debug && <McpDebugSession key={`${row.key}:${credential}`} prepare={prepare} />}
  </Card>;
}

export default function McpServers({ credentialsOnly = false }: { credentialsOnly?: boolean }) {
  const app = useApp(); const qc = useQueryClient();
  const queryKey = ["mcp-configuration", app.workspaceId];
  const data = useQuery({ queryKey, queryFn: () => api.get<McpConfigurationView>(ws("/v1/config/mcp-servers")), retry: false });
  const [selected, setSelected] = useState("");
  const [editing, setEditing] = useState<McpDefinition | null | undefined>();
  const [sourceUrl, setSourceUrl] = useState("");
  useEffect(() => setSourceUrl(data.data?.catalog.source_url ?? ""), [data.data?.catalog.source_url]);
  const refresh = () => { void qc.invalidateQueries({ queryKey }); };
  const save = useMutation({ mutationFn: (definition: McpDefinition) => api.put(ws("/v1/config/mcp-servers"), { expected_revision: data.data!.revision, definition }), onSuccess: (_value, definition) => { setEditing(undefined); setSelected(`local:${definition.id}`); refresh(); } });
  const remove = useMutation({ mutationFn: (key: string) => api.post(ws(`/v1/config/mcp-servers/${encodeURIComponent(key)}/remove`), { expected_revision: data.data!.revision }), onSuccess: () => { setSelected(""); refresh(); } });
  const source = useMutation({ mutationFn: () => api.put(ws("/v1/config/mcp-catalog/source"), { expected_revision: data.data!.revision, url: sourceUrl.trim() || null }), onSuccess: refresh });
  const sync = useMutation({ mutationFn: () => api.post(ws("/v1/config/mcp-catalog/sync")), onSuccess: refresh });
  const current = data.data?.servers.find((row) => row.key === selected);
  const error = data.error || save.error || remove.error || source.error || sync.error;
  return <div className="stack">
    <p className="hint">{app.t("Choose a server, select the account to use, and test it. Credentials stay in their existing Vault.", "选择服务器、使用账户并进行测试。凭证仍保存在原有 Vault 中。")}</p>
    {!credentialsOnly && <div className="row"><Button variant="primary" onClick={() => { save.reset(); setEditing(null); }}>{app.t("Add MCP server", "添加 MCP 服务器")}</Button>
      <Link to={`/w/${app.workspaceId}/vaults`}>{app.t("Manage Vaults", "管理 Vault")}</Link></div>}
    {error instanceof Error && <div className="err">{error.message}</div>}
    <div className="template-grid">
      {data.data?.servers.map((row) => <button className="template-card" key={row.key} data-selected={selected === row.key} onClick={() => setSelected(row.key)}>
        <strong>{row.definition.name}</strong><span>{row.definition.description}</span><small>{row.definition.transport.type === "url" ? "HTTP" : "Sandbox stdio"} · {row.origin}</small>
      </button>)}
    </div>
    {current && <><ServerDetail key={`${current.key}:${current.target_fingerprint}`} row={current} revision={data.data!.revision} onChanged={refresh} />
      {!credentialsOnly && current.origin === "local" && <div className="row">
        <Button onClick={() => { save.reset(); setEditing(current.definition); }}>{app.t("Edit server", "编辑服务器")}</Button>
        <Button disabled={remove.isPending} onClick={() => remove.mutate(current.key)}>{app.t("Remove local definition", "移除本地定义")}</Button>
      </div>}</>}
    {editing !== undefined && <McpServerForm initial={editing ?? undefined} pending={save.isPending} error={save.error instanceof Error ? save.error : null} onSave={(definition) => save.mutate(definition)} onClose={() => setEditing(undefined)} />}
    {!credentialsOnly && <details><summary>{app.t("Catalog synchronization", "目录同步")}</summary><div className="stack">
      <p className="hint">{app.t("Optional HTTPS catalog source. Background refresh runs every 5 minutes. Failed downloads retain the last accepted catalog; local definitions and account defaults are not overwritten.", "可选 HTTPS 目录源，每 5 分钟后台刷新。下载失败保留上次有效目录；本地定义和默认账户不被覆盖。")}</p>
      <TextField label={app.t("Catalog source URL", "目录源 URL")} value={sourceUrl} onChange={(e) => setSourceUrl(e.target.value)} placeholder="https://…/mcp-catalog.json" />
      <div className="row"><Button disabled={source.isPending || !data.data} onClick={() => source.mutate()}>{app.t("Save catalog source", "保存目录源")}</Button>
        <Button disabled={sync.isPending || !data.data?.catalog.source_url} onClick={() => sync.mutate()}>{app.t("Sync now", "立即同步")}</Button></div>
      <span className="mut">{app.t("Catalog revision", "目录修订")} {data.data?.catalog.revision}</span>
    </div></details>}
    {!credentialsOnly && <details><summary>{app.t("Existing Agent bindings", "已有 Agent 绑定")}</summary><McpAgentInventory /></details>}
  </div>;
}
