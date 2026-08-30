import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useParams } from "react-router";
import DreamCreateModal from "../components/memory/DreamCreateModal";
import { DreamStatusPill } from "../components/memory/DreamList";
import { Button, Card, EmptyState, Modal, Pill, TechnicalId, useConfirm, useToast } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { AgentConfigList, Dream, MemoryEntry, Page } from "../lib/api/types";
import { dreamMemoryStoreId, dreamSessionIds, isDreamTerminal } from "../lib/dreams";
import { useApp } from "../lib/app-state";
import { visibleAgents } from "../lib/visible-agents";
import { useState } from "react";
import { dateTimeLabel, entityDisplayName, identifierLabel } from "../lib/presentation";

function tokenTotal(dream: Dream): number {
  return dream.usage.input_tokens + dream.usage.output_tokens + dream.usage.cache_creation_input_tokens + dream.usage.cache_read_input_tokens;
}

export default function DreamDetailSurface() {
  const app = useApp();
  const nav = useNavigate();
  const qc = useQueryClient();
  const toast = useToast();
  const confirm = useConfirm();
  const { ws: wsId = "default", dreamId = "" } = useParams();
  const [useOutput, setUseOutput] = useState(false);
  const [repeat, setRepeat] = useState(false);
  const dream = useQuery({
    queryKey: ["dream", dreamId],
    queryFn: () => api.get<Dream>(ws(`/v1/dreams/${dreamId}`)),
    refetchInterval: (query) => ["pending", "running"].includes(query.state.data?.status ?? "") ? 2_500 : false,
  });
  const item = dream.data;
  const sourceId = item ? dreamMemoryStoreId(item) : undefined;
  const outputId = item?.outputs[0]?.memory_store_id;
  const source = useQuery({ queryKey: ["dream-compare", sourceId], enabled: !!sourceId, queryFn: () => api.get<Page<MemoryEntry>>(ws(`/v1/memory_stores/${sourceId}/memories?view=full&limit=100`)) });
  const output = useQuery({ queryKey: ["dream-compare", outputId], enabled: !!outputId, queryFn: () => api.get<Page<MemoryEntry>>(ws(`/v1/memory_stores/${outputId}/memories?view=full&limit=100`)) });
  const agents = useQuery({ queryKey: ["config-agents", wsId], enabled: useOutput, queryFn: () => api.get<AgentConfigList>(ws("/v1/config/agents")) });
  const cancel = useMutation({
    mutationFn: () => api.post<Dream>(ws(`/v1/dreams/${dreamId}/cancel`)),
    onSuccess: (result) => { qc.setQueryData(["dream", dreamId], result); void qc.invalidateQueries({ queryKey: ["dreams"] }); toast.ok(app.t("Dream canceled.", "Dream 已取消。")); },
    onError: (error) => toast.err(error instanceof Error ? error.message : String(error)),
  });
  const archive = useMutation({
    mutationFn: () => api.post<Dream>(ws(`/v1/dreams/${dreamId}/archive`)),
    onSuccess: (result) => { qc.setQueryData(["dream", dreamId], result); void qc.invalidateQueries({ queryKey: ["dreams"] }); toast.ok(app.t("Dream archived.", "Dream 已归档。")); },
    onError: (error) => toast.err(error instanceof Error ? error.message : String(error)),
  });
  if (dream.isLoading) return <Card><div className="mut">{app.t("Loading Dream…", "正在加载 Dream…")}</div></Card>;
  if (dream.error instanceof Error || !item) return <Card><EmptyState title={app.t("Dream could not be loaded", "Dream 加载失败")} hint={dream.error instanceof Error ? dream.error.message : dreamId} action={<Button onClick={() => nav(`/w/${wsId}/memory`)}>{app.t("Back to Memory", "返回 Memory")}</Button>} /></Card>;
  const sourceByPath = new Map((source.data?.data ?? []).map((memory) => [memory.path, memory]));
  const outputByPath = new Map((output.data?.data ?? []).map((memory) => [memory.path, memory]));
  const changes = Array.from(new Set([...sourceByPath.keys(), ...outputByPath.keys()])).sort().map((path) => {
    const before = sourceByPath.get(path); const after = outputByPath.get(path);
    return { path, kind: !before ? "added" : !after ? "removed" : before.content_sha256 === after.content_sha256 ? "unchanged" : "changed" } as const;
  }).filter((change) => change.kind !== "unchanged");
  return <div className="stack dream-detail">
    <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
      <div><div className="row"><Button variant="ghost" onClick={() => nav(`/w/${wsId}/memory`)}>← {app.t("Memory", "记忆")}</Button><h2 style={{ margin: 0 }}>{app.t("Dream run", "Dream 运行")}</h2><DreamStatusPill status={item.status} />{item.archived_at && <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>}<Pill tone="info">{app.t("research preview", "研究预览")}</Pill></div><TechnicalId value={item.id} /><div className="mut">{dateTimeLabel(item.created_at, app.locale)}{item.ended_at ? ` → ${dateTimeLabel(item.ended_at, app.locale)}` : ""}</div></div>
      <div className="row">
        {(item.status === "pending" || item.status === "running") && <Button variant="danger" disabled={cancel.isPending} onClick={async () => { if (await confirm({ title: app.t("Cancel this Dream?", "取消这次 Dream？"), body: app.t("Execution stops, but an already-created output store remains available for review.", "运行会停止，但已创建的输出记忆库仍可检查。"), confirmLabel: app.t("Cancel Dream", "取消 Dream"), danger: true })) cancel.mutate(); }}>{app.t("Cancel", "取消")}</Button>}
        {isDreamTerminal(item.status) && !item.archived_at && <Button disabled={archive.isPending} onClick={() => archive.mutate()}>{app.t("Archive", "归档")}</Button>}
        <Button onClick={() => setRepeat(true)}>{app.t("Run again", "再次运行")}</Button>
      </div>
    </div>
    {item.error && <div className="notice danger"><strong>{item.error.type}</strong><div>{item.error.message}</div></div>}
    <div className="dream-summary-grid">
      <Card><span className="mut">{app.t("Source store", "来源记忆库")}</span><div><strong>{sourceId ? app.t("Memory Store", "记忆库") : app.t("Not available", "不可用")}</strong>{sourceId && <TechnicalId value={sourceId} />}</div>{sourceId && <Button onClick={() => nav(`/w/${wsId}/memory?store=${encodeURIComponent(sourceId)}`)}>{app.t("Open source", "打开来源")}</Button>}</Card>
      <Card><span className="mut">{app.t("Evidence Sessions", "证据会话")}</span><div><strong>{dreamSessionIds(item).length}</strong> {app.t("Sessions", "个会话")}</div><div className="row dream-id-list">{dreamSessionIds(item).slice(0, 8).map((id, index) => <button className="manage-link" title={id} key={id} onClick={() => nav(`/w/${wsId}/sessions/${id}`)}>{app.t(`Session ${index + 1}`, `会话 ${index + 1}`)}</button>)}</div></Card>
      <Card><span className="mut">{app.t("Model usage", "模型用量")}</span><div>{identifierLabel(item.model.id)}</div><div>{tokenTotal(item).toLocaleString()} {app.t("tokens", "个 Token")}</div></Card>
      <Card><span className="mut">{app.t("Output store", "输出记忆库")}</span><div><strong>{outputId ? app.t("Memory Store", "记忆库") : app.t("Not created yet", "尚未创建")}</strong>{outputId && <TechnicalId value={outputId} />}</div>{outputId && <div className="row"><Button variant="primary" onClick={() => nav(`/w/${wsId}/memory?store=${encodeURIComponent(outputId)}`)}>{app.t("Open output", "打开输出")}</Button><Button onClick={() => setUseOutput(true)}>{app.t("Use in Agent", "用于 Agent")}</Button></div>}</Card>
    </div>
    {item.session_id && <Card><div className="row" style={{ justifyContent: "space-between" }}><div><strong>{app.t("Execution Session", "运行会话")}</strong><TechnicalId value={item.session_id} /></div><Button onClick={() => nav(`/w/${wsId}/sessions/${item.session_id}`)}>{app.t("Open run details", "打开运行详情")} →</Button></div></Card>}
    <Card><strong>{app.t("Guidance", "整理指导")}</strong><pre className="mono dream-guidance">{item.instructions || app.t("Default Dream curation guidance", "默认 Dream 整理指导")}</pre></Card>
    {outputId && <Card style={{ padding: 0, overflow: "hidden" }}><div className="row" style={{ padding: 12, justifyContent: "space-between" }}><strong>{app.t("Source → output changes", "来源 → 输出变化")}</strong><span className="mut">{changes.length} {app.t("changed paths", "个路径变化")}</span></div><table className="table"><thead><tr><th>{app.t("Path", "路径")}</th><th>{app.t("Change", "变化")}</th></tr></thead><tbody>{changes.map((change) => <tr key={change.path}><td className="mono">{change.path}</td><td><Pill tone={change.kind === "removed" ? "danger" : change.kind === "added" ? "ok" : "warn"}>{app.t(change.kind === "removed" ? "removed" : change.kind === "added" ? "added" : "changed", change.kind === "removed" ? "已移除" : change.kind === "added" ? "已新增" : "已修改")}</Pill></td></tr>)}{!source.isLoading && !output.isLoading && changes.length === 0 && <tr><td colSpan={2} className="mut">{app.t("No content changes detected.", "未发现内容变化。")}</td></tr>}</tbody></table></Card>}
    <div className="row" style={{ justifyContent: "flex-end" }}><Button variant="ghost" onClick={() => nav(`/w/${wsId}/agents/new?template=dream&stage=build&section=instructions`)}>⚙ {app.t("Customize Dream Agent", "自定义 Dream Agent")}</Button></div>
    {useOutput && outputId && <Modal title={app.t("Use output in Agent", "将输出用于 Agent")} onClose={() => setUseOutput(false)}><p className="mut">{app.t("Choose an Agent. The output Store is added to its draft for review; save and publish the Agent when the binding is correct.", "选择 Agent。输出记忆库会加入该 Agent 草稿供检查；确认绑定正确后保存并发布 Agent。")}</p><div className="stack">{visibleAgents(agents.data?.data).map((agent) => <Button title={agent.id} key={agent.id} onClick={() => nav(`/w/${wsId}/agents/${agent.id}?stage=build&section=knowledge&memory_store=${encodeURIComponent(outputId)}`)}>{entityDisplayName(agent.name, identifierLabel(agent.id))}</Button>)}{!agents.isLoading && visibleAgents(agents.data?.data).length === 0 && <EmptyState title={app.t("No authored Agents are available", "没有可用的已创作 Agent")} hint={agents.error instanceof Error ? agents.error.message : app.t("Create an Agent and attach this Dream output as its first Memory resource.", "创建 Agent，并将该 Dream 输出作为首个 Memory 资源。") } action={<Button variant="primary" onClick={() => nav(`/w/${wsId}/agents/new?stage=build&section=knowledge&memory_store=${encodeURIComponent(outputId)}`)}>+ {app.t("Create Agent", "创建 Agent")}</Button>} />}</div></Modal>}
    {repeat && <DreamCreateModal initialStoreId={sourceId} onClose={() => setRepeat(false)} />}
  </div>;
}
