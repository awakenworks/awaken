// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: Detail view of one authored MCP server — definition, default choices and binding preview
// @formal reason: — all projected from the config plane it edits through. Classification only, not formal
// @formal reason: evidence.
import { useId, useState } from "react";
import { useMutation, useQuery } from "@tanstack/react-query";
import { Button, Card, InlineNotice } from "@awaken/ui";
import { useNavigate } from "react-router";
import { api, ws } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import type { McpInspection, McpPreparedBinding, McpServerRow } from "../../lib/api/mcp-types";
import type { Environment, Page } from "../../lib/api/types";
import type { McpStdioToolPreview } from "../../lib/api/mcp-types";
import { mcpAddress } from "../../lib/api/mcp-types";
import McpCredentialSelect from "./McpCredentialSelect";
import McpDebugSession from "./McpDebugSession";

export default function McpServerDetail({ row, revision, onChanged }: { row: McpServerRow; revision: number; onChanged: () => void }) {
  const app = useApp(); const nav = useNavigate(); const defaultHintId = useId();
  const [credential, setCredential] = useState(row.default_matches_target ? row.default_credential?.vault_credential_id ?? "" : "");
  const [debug, setDebug] = useState(false);
  const [inspection, setInspection] = useState<McpInspection>();
  const base = ws(`/v1/config/mcp-servers/${encodeURIComponent(row.key)}`);
  const selected = () => ({ expected_revision: revision, credential_id: credential || null });
  const check = useMutation({ mutationFn: () => api.post<McpInspection>(`${base}/inspect`, selected()), onSuccess: setInspection });
  const saveDefault = useMutation({ mutationFn: () => api.put(`${base}/default-credential`, selected()), onSuccess: onChanged });
  const prepare = () => api.post<McpPreparedBinding>(`${base}/prepare`, selected());
  return <Card className="ui-stack mcp-detail">
    <div className="row">
      {row.definition.icon ? <img src={row.definition.icon} alt="" width={28} height={28} /> : <span aria-hidden="true" className="badge">{row.definition.name.slice(0, 1)}</span>}
      <strong>{row.definition.name}</strong><span className="mut">{row.origin === "local" ? app.t("Local configuration", "本地配置") : app.t("Catalog preset", "目录预设")}</span>
    </div>
    <p>{row.definition.description}</p><code style={{ overflowWrap: "anywhere" }}>{mcpAddress(row.definition)}</code>
    {!row.default_matches_target && <InlineNotice tone="warning">{app.t("The endpoint changed. Choose a matching credential again.", "目标地址已变化，请重新选择匹配凭证。")}</InlineNotice>}
    <McpCredentialSelect disabled={check.isPending || saveDefault.isPending || debug} url={row.definition.transport.type === "url" ? row.definition.transport.url : undefined} value={credential} onChange={(choice) => { setCredential(choice?.id ?? ""); setInspection(undefined); check.reset(); saveDefault.reset(); setDebug(false); }} />
    {row.definition.transport.type === "sandbox_stdio" && <p className="hint">{app.t("Sandbox stdio runs in the selected Session environment. This configuration does not inject Vault secrets into process arguments or environment variables.", "Sandbox stdio 在所选 Session 环境中运行。此配置不会将 Vault 秘密注入进程参数或环境变量。")}</p>}
    {row.definition.transport.type === "sandbox_stdio" && <StdioToolPreview key={revision} base={base} revision={revision} />}
    <p id={defaultHintId} className="hint">{app.t("The default preselects an account when adding this server to an Agent. It does not change existing bindings.", "默认选择会在向 Agent 添加此服务器时预选账户，不会修改已有绑定。")}</p>
    <div className="row" style={{ flexWrap: "wrap" }}>
      <Button disabled={check.isPending || debug || row.definition.transport.type !== "url"} onClick={() => { setInspection(undefined); check.mutate(); }}>{app.t(check.isPending ? "Checking…" : "Check connection", check.isPending ? "检查中…" : "检查连接")}</Button>
      <Button aria-describedby={defaultHintId} disabled={saveDefault.isPending || check.isPending || debug} onClick={() => saveDefault.mutate()}>{app.t("Save default credential", "保存默认凭证")}</Button>
      <Button disabled={check.isPending || saveDefault.isPending} onClick={() => setDebug(true)}>{app.t("Test with an Agent", "使用 Agent 试运行")}</Button>
      <Button disabled={check.isPending || saveDefault.isPending} onClick={() => nav(`/w/${app.workspaceId}/agents/new?mcp=${encodeURIComponent(row.key)}`)}>{app.t("Use in a new Agent", "用于新 Agent")}</Button>
    </div>
    {saveDefault.isSuccess && <span role="status" className="mut">{app.t("Default saved. Existing Agents and Sessions are unchanged.", "默认选择已保存，已有 Agent 和 Session 保持不变。")}</span>}
    {(check.error || saveDefault.error) instanceof Error && <div role="alert" className="err">{(check.error || saveDefault.error)?.message}<Button onClick={() => { onChanged(); check.reset(); saveDefault.reset(); }}>{app.t("Reload configuration", "重新加载配置")}</Button></div>}
    {inspection && <div className="ui-stack" aria-label={app.t("Connection test result", "连接测试结果")}>
      <strong role="status">{inspection.status} {inspection.http_status ?? ""}</strong>
      <span className="mut">{app.t("Tested from Control", "测试位置：Control")} · {inspection.checked_at} · {inspection.credential?.display_name ?? app.t("Anonymous", "匿名")}</span>
      <p className="hint">{app.t("This checks authentication, initialization and tool discovery. Session execution still verifies its own environment.", "这里检查认证、初始化和工具发现；Session 执行仍需验证其实际环境。")}</p>
      {inspection.tools.map((tool) => <details key={tool.name}><summary>{tool.name}</summary><p>{tool.description}</p><pre style={{ overflow: "auto" }}>{JSON.stringify(tool.input_schema, null, 2)}</pre></details>)}
    </div>}
    {debug && <p className="hint">{app.t("The account is fixed for this debug session. Return to the server list to test a different account.", "本次调试固定使用当前账户。返回服务器列表后可测试其他账户。")}</p>}
    {debug && <McpDebugSession key={`${row.key}:${credential}`} prepare={prepare} />}
  </Card>;
}

function StdioToolPreview({ base, revision }: { base: string; revision: number }) {
  const app = useApp();
  const [environment, setEnvironment] = useState("");
  const environments = useQuery({
    queryKey: ["environments", app.workspaceId],
    queryFn: () => api.get<Page<Environment>>(ws("/v1/environments")),
  });
  const preview = useQuery({
    queryKey: ["mcp-stdio-tools", app.workspaceId, base, revision, environment],
    queryFn: () => api.get<McpStdioToolPreview>(`${base}/tools?${new URLSearchParams({ expected_revision: String(revision), environment_id: environment })}`),
    retry: false,
  });
  const current = preview.data;
  const labels = {
    configured: app.t("From configuration", "配置提供"),
    discovered: app.t("Recently discovered", "最近发现"),
    stale: app.t("Discovery is stale", "发现结果已过期"),
    pending: app.t("Tools have not been discovered", "尚未发现工具"),
  };
  return <section className="ui-stack" aria-label={app.t("Tool preview", "工具预览")}>
    <label className="ui-field"><span>{app.t("Preview environment", "预览环境")}</span>
      <select className="ui-input" value={environment} onChange={(event) => setEnvironment(event.target.value)}>
        <option value="">{app.t("Choose an environment", "选择环境")}</option>
        {environments.data?.data.filter((item) => !item.archived_at).map((item) => <option key={item.id} value={item.id}>{item.name || item.id}</option>)}
      </select>
    </label>
    {(environments.isError || preview.isError) && <p role="alert">{app.t("Tool preview is unavailable", "工具预览暂不可用")}</p>}
    {current && <>
      <p role="status">{labels[current.source]}{current.observed_at_unix_ms != null && <> · <time dateTime={new Date(current.observed_at_unix_ms).toISOString()}>{new Date(current.observed_at_unix_ms).toLocaleString()}</time></>}</p>
      {!current.tools.length && current.source === "discovered" && <p>{app.t("No tools found", "确认无工具")}</p>}
      {current.tools.map((tool) => <details key={tool.name}><summary>{tool.name}</summary><p>{tool.description}</p><pre style={{ overflow: "auto" }}>{JSON.stringify(tool.input_schema, null, 2)}</pre></details>)}
    </>}
  </section>;
}
