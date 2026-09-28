// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: The Workspace MCP server list and its create/remove entry points, projected from the
// @formal reason: config plane. Classification only, not formal evidence.
import { useEffect, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "@awaken/ui";
import { Link } from "react-router";
import { api, ws } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import type { McpConfigurationView, McpDefinition } from "../../lib/api/mcp-types";
import { mcpAddress } from "../../lib/api/mcp-types";
import McpServerForm from "./McpServerForm";
import McpServerDetail from "./McpServerDetail";
import McpAgentInventory from "./McpAgentInventory";
import { TextField, useConfirm } from "../ui";

export default function McpServers({ credentialsOnly = false }: { credentialsOnly?: boolean }) {
  const app = useApp(); const confirm = useConfirm(); const qc = useQueryClient();
  const queryKey = ["mcp-configuration", app.workspaceId];
  const data = useQuery({ queryKey, queryFn: () => api.get<McpConfigurationView>(ws("/v1/config/mcp-servers")), retry: false });
  const [search, setSearch] = useState("");
  const [selected, setSelected] = useState("");
  const [editing, setEditing] = useState<McpDefinition | null | undefined>();
  const [sourceUrl, setSourceUrl] = useState("");
  useEffect(() => setSourceUrl(data.data?.catalog.source_url ?? ""), [data.data?.catalog.source_url]);
  const refresh = () => { void qc.invalidateQueries({ queryKey }); };
  const reload = async () => { await data.refetch(); save.reset(); remove.reset(); source.reset(); sync.reset(); };
  const save = useMutation({ mutationFn: (definition: McpDefinition) => api.put(ws("/v1/config/mcp-servers"), { expected_revision: data.data!.revision, definition }), onSuccess: (_value, definition) => { setEditing(undefined); setSelected(`local:${definition.id}`); refresh(); } });
  const remove = useMutation({ mutationFn: (key: string) => api.post(ws(`/v1/config/mcp-servers/${encodeURIComponent(key)}/remove`), { expected_revision: data.data!.revision }), onSuccess: () => { setSelected(""); refresh(); } });
  const source = useMutation({ mutationFn: () => api.put(ws("/v1/config/mcp-catalog/source"), { expected_revision: data.data!.revision, url: sourceUrl.trim() || null }), onSuccess: refresh });
  const sync = useMutation({ mutationFn: () => api.post(ws("/v1/config/mcp-catalog/sync")), onSuccess: refresh });
  const filtered = data.data?.servers.filter((row) => `${row.definition.name} ${row.definition.description} ${mcpAddress(row.definition)}`.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));
  const current = data.data?.servers.find((row) => row.key === selected);
  const error = data.error || save.error || remove.error || source.error || sync.error;
  return <div className="ui-stack mcp-config">
    <p className="hint">{app.t("Choose a server, select the account to use, and test it. Credentials stay in their existing Vault.", "选择服务器、使用账户并进行测试。凭证仍保存在原有 Vault 中。")}</p>
    {!credentialsOnly && <div className="row"><Button variant="primary" disabled={!data.data || data.isError} onClick={() => { save.reset(); setEditing(null); }}>{app.t("Add MCP server", "添加 MCP 服务器")}</Button>
      <Link to={`/w/${app.workspaceId}/vaults`}>{app.t("Manage Vaults", "管理 Vault")}</Link></div>}
    {data.isPending && <p role="status">{app.t("Loading MCP servers…", "正在加载 MCP 服务器…")}</p>}
    {error instanceof Error && <div role="alert" className="err">{error.message}<p>{app.t("Reload the latest configuration, review your changes, then retry. Draft fields are kept.", "请重新加载最新配置，核对修改后重试。草稿字段会保留。")}</p><Button disabled={data.isFetching} onClick={() => void reload()}>{app.t("Reload configuration", "重新加载配置")}</Button></div>}
    <TextField label={app.t("Search MCP servers", "搜索 MCP 服务器")} value={search} onChange={(e) => setSearch(e.target.value)} />
    {data.isSuccess && !filtered?.length && <p role="status">{app.t(search ? "No servers match your search." : "No MCP servers configured yet.", search ? "没有符合搜索条件的服务器。" : "尚未配置 MCP 服务器。")}</p>}
    <div className="template-grid">
      {filtered?.map((row) => <button className="template-card" key={row.key} aria-pressed={selected === row.key} data-selected={selected === row.key} onClick={() => setSelected(row.key)}>
        <strong>{row.definition.name}</strong><span>{row.definition.description}</span><small>{row.definition.transport.type === "url" ? "HTTP" : "Sandbox stdio"} · {row.origin}</small>
      </button>)}
    </div>
    {current && <><Button onClick={() => setSelected("")}>{app.t("Back to server list", "返回服务器列表")}</Button><McpServerDetail key={`${current.key}:${current.target_fingerprint}`} row={current} revision={data.data!.revision} onChanged={refresh} />
      {!credentialsOnly && current.origin === "local" && <div className="row">
        <Button onClick={() => { save.reset(); setEditing(current.definition); }}>{app.t("Edit server", "编辑服务器")}</Button>
        <Button disabled={remove.isPending} onClick={async () => {
          if (await confirm({ title: app.t("Remove this local definition?", "移除此本地定义？"), body: app.t("Its default account selection is removed too. Published Agents and Sessions keep their existing bindings.", "默认账户选择也将移除。已发布 Agent 和 Session 保留原有绑定。"), confirmLabel: app.t("Remove definition", "移除定义"), danger: true })) remove.mutate(current.key);
        }}>{app.t("Remove local definition", "移除本地定义")}</Button>
      </div>}</>}
    {editing !== undefined && <McpServerForm onReload={() => void reload()} key={editing?.id ?? "new"} initial={editing ?? undefined} pending={save.isPending} error={save.error instanceof Error ? save.error : null} onSave={(definition) => save.mutate(definition)} onClose={() => setEditing(undefined)} />}
    {sync.isSuccess && <p role="status">{app.t("Catalog synchronized.", "目录已同步。")}</p>}
    {source.isSuccess && <p role="status">{app.t("Catalog source saved.", "目录源已保存。")}</p>}
    {!credentialsOnly && <details><summary>{app.t("Catalog synchronization", "目录同步")}</summary><div className="ui-stack">
      <p className="hint">{app.t("Optional HTTPS catalog source. Background refresh runs every 5 minutes. Failed downloads retain the last accepted catalog; local definitions and account defaults are not overwritten.", "可选 HTTPS 目录源，每 5 分钟后台刷新。下载失败保留上次有效目录；本地定义和默认账户不被覆盖。")}</p>
      <TextField label={app.t("Catalog source URL", "目录源 URL")} value={sourceUrl} disabled={source.isPending || sync.isPending} onChange={(e) => { setSourceUrl(e.target.value); source.reset(); sync.reset(); }} placeholder="https://…/mcp-catalog.json" />
      <div className="row"><Button disabled={source.isPending || !data.data} onClick={() => source.mutate()}>{app.t("Save catalog source", "保存目录源")}</Button>
        <Button disabled={sync.isPending || !data.data?.catalog.source_url} onClick={() => sync.mutate()}>{app.t("Sync now", "立即同步")}</Button></div>
      <span className="mut">{app.t("Catalog revision", "目录修订")} {data.data?.catalog.revision}</span>
    </div></details>}
    {!credentialsOnly && <details><summary>{app.t("Existing Agent bindings", "已有 Agent 绑定")}</summary><McpAgentInventory /></details>}
  </div>;
}
