import { useInfiniteQuery } from "@tanstack/react-query";
import { useState } from "react";
import { useNavigate, useParams } from "react-router";
import type { DreamPage, DreamStatus } from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import { dreamMemoryStoreId, dreamSessionIds } from "../../lib/dreams";
import { useApp } from "../../lib/app-state";
import { Button, Card, EmptyState, Pill, Segmented, SkeletonRows, TextField } from "../ui";
import DreamCreateModal from "./DreamCreateModal";

export function DreamStatusPill({ status }: { status: DreamStatus }) {
  const tone = status === "completed" ? "ok" : status === "failed" ? "danger" : status === "canceled" ? "neutral" : status === "running" ? "agent" : "info";
  return <Pill tone={tone} dot={status === "running"}>{status}</Pill>;
}

export default function DreamList({ storeId }: { storeId?: string }) {
  const app = useApp();
  const nav = useNavigate();
  const { ws: wsId = "default" } = useParams();
  const [filter, setFilter] = useState<"all" | "active" | "failed">("all");
  const [archived, setArchived] = useState(false);
  const [createdAfter, setCreatedAfter] = useState("");
  const [createdBefore, setCreatedBefore] = useState("");
  const [creating, setCreating] = useState(false);
  const queryParams = (page: string | null) => {
    const params = new URLSearchParams({ limit: "25", include_archived: String(archived) });
    if (filter === "active") { params.append("statuses", "pending"); params.append("statuses", "running"); }
    if (filter === "failed") params.append("statuses", "failed");
    if (createdAfter) params.set("created_at[gt]", new Date(createdAfter).toISOString());
    if (createdBefore) params.set("created_at[lt]", new Date(createdBefore).toISOString());
    if (page) params.set("page", page);
    return params;
  };
  const dreams = useInfiniteQuery({
    queryKey: ["dreams", filter, archived, createdAfter, createdBefore],
    initialPageParam: null as string | null,
    queryFn: ({ pageParam }) => api.get<DreamPage>(ws(`/v1/dreams?${queryParams(pageParam)}`)),
    getNextPageParam: (lastPage) => lastPage.next_page ?? undefined,
    refetchInterval: (query) => query.state.data?.pages.some((page) => page.data.some((dream) => dream.status === "pending" || dream.status === "running")) ? 3_000 : 30_000,
  });
  const rows = (dreams.data?.pages.flatMap((page) => page.data) ?? []).filter((dream) => !storeId
    || dreamMemoryStoreId(dream) === storeId
    || dream.outputs.some((output) => output.memory_store_id === storeId));
  return <div className="stack">
    <div className="row" style={{ justifyContent: "space-between" }}>
      <span className="row">
        <Segmented value={filter} onChange={setFilter} options={[
          { value: "all", label: app.t("All", "全部") },
          { value: "active", label: app.t("Active", "运行中") },
          { value: "failed", label: app.t("Failed", "失败") },
        ]} />
        <label className="row mut"><input type="checkbox" checked={archived} onChange={(event) => setArchived(event.target.checked)} />{app.t("Archived", "已归档")}</label>
      </span>
      <Button variant="primary" onClick={() => setCreating(true)}>+ {app.t("Start Dream", "启动 Dream")}</Button>
    </div>
    <div className="row dream-date-filters">
      <TextField type="datetime-local" label={app.t("Created after", "创建晚于")} value={createdAfter} onChange={(event) => setCreatedAfter(event.target.value)} />
      <TextField type="datetime-local" label={app.t("Created before", "创建早于")} value={createdBefore} onChange={(event) => setCreatedBefore(event.target.value)} />
      {(createdAfter || createdBefore) && <Button onClick={() => { setCreatedAfter(""); setCreatedBefore(""); }}>{app.t("Clear dates", "清除时间")}</Button>}
    </div>
    <Card style={{ padding: 0, overflow: "hidden" }}>
      <div className="grid-scroll"><table className="table">
        <thead><tr><th>Dream</th><th>{app.t("Status", "状态")}</th><th>{app.t("Source", "来源")}</th><th>{app.t("Sessions", "会话")}</th><th>{app.t("Model", "模型")}</th><th>{app.t("Created", "创建时间")}</th><th>{app.t("Output", "输出")}</th></tr></thead>
        {dreams.isLoading ? <SkeletonRows rows={4} cols={7} /> : <tbody>
          {rows.map((dream) => <tr key={dream.id} data-click="true" onClick={() => nav(`/w/${wsId}/memory/dreams/${dream.id}`)}>
            <td className="mono">{dream.id}</td><td><DreamStatusPill status={dream.status} /></td>
            <td className="mono mut">{dreamMemoryStoreId(dream) ?? "—"}</td><td>{dreamSessionIds(dream).length}</td>
            <td className="mono">{dream.model.id}</td><td className="mut">{new Date(dream.created_at).toLocaleString()}</td>
            <td className="mono mut">{dream.outputs[0]?.memory_store_id ?? "—"}</td>
          </tr>)}
          {rows.length === 0 && <tr><td colSpan={7}><EmptyState title={app.t("No Dreams yet", "还没有 Dream")} hint={app.t("Start one from reviewed Sessions and a source Memory Store.", "从已检查的会话和来源记忆库启动一次 Dream。")}/></td></tr>}
        </tbody>}
      </table></div>
    </Card>
    {dreams.hasNextPage && <div className="row" style={{ justifyContent: "center" }}>
      <Button disabled={dreams.isFetchingNextPage} onClick={() => dreams.fetchNextPage()}>
        {dreams.isFetchingNextPage ? app.t("Loading…", "加载中…") : app.t("Load more", "加载更多")}
      </Button>
    </div>}
    {dreams.error instanceof Error && <div className="err">{dreams.error.message}</div>}
    {creating && <DreamCreateModal initialStoreId={storeId} onClose={() => setCreating(false)} />}
  </div>;
}
