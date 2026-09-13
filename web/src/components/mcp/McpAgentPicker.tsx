import { useEffect, useRef, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { Button, Card } from "@awaken/ui";
import { api, ws } from "../../lib/api/client";
import type { AgentConfig, ManagedToolsetCap } from "../../lib/api/types";
import type { McpConfigurationView, McpPreparedBinding } from "../../lib/api/mcp-types";
import { mcpDefaultConfig, reconcileMcpToolsets } from "../../lib/agent-toolsets";
import { useApp } from "../../lib/app-state";
import McpCredentialSelect from "./McpCredentialSelect";

export default function McpAgentPicker({ config, toolsets, initialKey, onPatch, onVault }: {
  config: AgentConfig; toolsets?: ManagedToolsetCap[]; initialKey?: string;
  onPatch: (patch: Partial<AgentConfig>) => void; onVault: (id: string) => void;
}) {
  const app = useApp();
  const latest = useRef(config); latest.current = config;
  const active = useRef(true);
  useEffect(() => { active.current = true; return () => { active.current = false; }; }, []);
  const catalog = useQuery({ queryKey: ["mcp-configuration", app.workspaceId], queryFn: () => api.get<McpConfigurationView>(ws("/v1/config/mcp-servers")) });
  const [key, setKey] = useState(initialKey ?? ""); const seeded = useRef(false);
  const [credential, setCredential] = useState(""); const [pin, setPin] = useState(false);
  const row = catalog.data?.servers.find((s) => s.key === key);
  const apply = useMutation({ mutationFn: async ({ target, credentialId, fixed }: { target: string; credentialId: string; fixed: boolean }) => {
    const prepared = await api.post<McpPreparedBinding>(ws(`/v1/config/mcp-servers/${encodeURIComponent(target)}/prepare`), { expected_revision: catalog.data!.revision, credential_id: credentialId || null });
    if (!active.current) return;
    const current = latest.current;
    if ((current.mcp_servers ?? []).some((s) => s.name === prepared.binding.name)) throw new Error(app.t("This server name already exists. Edit its existing binding.", "服务器名称已存在，请编辑原有绑定。"));
    const binding = { ...prepared.binding, credential: fixed && "credential" in prepared.binding ? prepared.binding.credential ?? undefined : undefined };
    const servers = [...(current.mcp_servers ?? []), binding];
    onPatch({
      id: current.id || `mcp-${binding.name}-${crypto.randomUUID().slice(0, 8)}`,
      name: current.name || `${binding.name} Agent`,
      mcp_servers: servers,
      tools: reconcileMcpToolsets(current.tools, servers, mcpDefaultConfig(toolsets)),
    });
    if (!fixed && prepared.credential) onVault(prepared.credential.vault_id);
  } });
  useEffect(() => {
    if (!initialKey || seeded.current || !catalog.data) return;
    const row = catalog.data.servers.find((s) => s.key === initialKey);
    if (!row) return;
    seeded.current = true;
    const credentialId = row.default_matches_target ? row.default_credential?.vault_credential_id ?? "" : "";
    setCredential(credentialId);
    apply.mutate({ target: initialKey, credentialId, fixed: false });
  }, [initialKey, catalog.data]);
  return <Card className="ui-stack">
    <strong>{app.t("Add a configured MCP server", "添加已配置 MCP 服务器")}</strong>
    <select className="ui-input" aria-label={app.t("Configured MCP server", "已配置 MCP 服务器")} value={key} disabled={apply.isPending} onChange={(e) => {
      setKey(e.target.value); setPin(false); const next = catalog.data?.servers.find((s) => s.key === e.target.value);
      setCredential(next?.default_matches_target ? next.default_credential?.vault_credential_id ?? "" : "");
      apply.reset();
    }}><option value="">{app.t("Choose a server…", "选择服务器…")}</option>
      {catalog.data?.servers.map((s) => <option key={s.key} value={s.key}>{s.definition.name} · {s.origin}</option>)}
    </select>
    {row && <>
      <McpCredentialSelect disabled={apply.isPending} url={row.definition.transport.type === "url" ? row.definition.transport.url : undefined} value={credential} onChange={(c) => setCredential(c?.id ?? "")} />
      <label><input type="checkbox" disabled={apply.isPending || !credential} checked={pin} onChange={(e) => setPin(e.target.checked)} /> {app.t("Pin this shared account to the Agent for all future Sessions", "将此共享账户固定到 Agent，供所有后续 Session 使用")}</label>
      <span className="mut">{app.t("By default the selected Vault is added only to the first run. Later Sessions choose their own identity.", "默认仅将所选 Vault 加入首次运行，后续 Session 独立选择身份。")}</span>
      <Button disabled={apply.isPending} onClick={() => apply.mutate({ target: key, credentialId: credential, fixed: pin })}>{app.t("Add to draft", "添加到草稿")}</Button>
    </>}
    {(apply.error || catalog.error) instanceof Error && <div role="alert" className="err">{(apply.error || catalog.error)?.message}<Button onClick={() => { void catalog.refetch(); apply.reset(); }}>{app.t("Reload configuration", "重新加载配置")}</Button></div>}
  </Card>;
}
