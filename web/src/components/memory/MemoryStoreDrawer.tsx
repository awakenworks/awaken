import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import type {
  MemoryEntry,
  MemoryStore,
  MemoryVersion,
  Page,
} from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import {
  Button,
  Card,
  Drawer,
  EmptyState,
  Pill,
  Segmented,
  SkeletonRows,
  useConfirm,
  useToast,
} from "../ui";

type Tab = "content" | "history";

function QueryError({ message, retry }: { message: string; retry: () => void }) {
  const app = useApp();
  return (
    <EmptyState
      title={app.t("This data could not be loaded", "数据加载失败")}
      hint={message}
      action={<Button onClick={retry}>{app.t("Try again", "重试")}</Button>}
    />
  );
}

export default function MemoryStoreDrawer({ store, onClose }: { store: MemoryStore; onClose: () => void }) {
  const app = useApp();
  const toast = useToast();
  const confirm = useConfirm();
  const qc = useQueryClient();
  const [tab, setTab] = useState<Tab>("content");
  const base = ws(`/v1/memory_stores/${encodeURIComponent(store.id)}`);
  const memories = useQuery({
    queryKey: ["memory-store-entries", store.id],
    queryFn: () => api.get<Page<MemoryEntry>>(`${base}/memories?view=full&limit=100`),
    enabled: tab === "content",
  });
  const versions = useQuery({
    queryKey: ["memory-store-versions", store.id],
    queryFn: () => api.get<Page<MemoryVersion>>(`${base}/memory_versions`),
    enabled: tab === "history",
  });

  const redact = useMutation({
    mutationFn: (versionId: string) => api.post(`${base}/memory_versions/${encodeURIComponent(versionId)}/redact`),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["memory-store-versions", store.id] });
      toast.ok(app.t("Version content redacted.", "版本内容已遮盖。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });

  return (
    <Drawer title={`${store.name || store.id} · ${app.t("Memory Store", "记忆库")}`} onClose={onClose}>
      <div className="stack">
        <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
          <div>
            <div className="mono">{store.id}</div>
            <div className="mut">{store.description || app.t("No description", "暂无描述")}</div>
          </div>
          {store.archived_at ? <Pill tone="neutral">{app.t("Archived", "已归档")}</Pill> : <Pill tone="ok">{app.t("Active", "使用中")}</Pill>}
        </div>

        <Segmented
          value={tab}
          onChange={setTab}
          options={[
            { value: "content", label: app.t("Content", "内容") },
            { value: "history", label: app.t("History", "版本历史") },
          ]}
        />

        {tab === "content" && memories.error instanceof Error && (
          <QueryError message={memories.error.message} retry={() => void memories.refetch()} />
        )}
        {tab === "content" && !memories.error && (
          <Card style={{ padding: 0, overflow: "auto" }}>
            <table className="table">
              <thead><tr><th>{app.t("Path", "路径")}</th><th>{app.t("Content", "内容")}</th><th>{app.t("Size", "大小")}</th><th>{app.t("Updated", "更新时间")}</th></tr></thead>
              {memories.isLoading ? <SkeletonRows rows={4} cols={4} /> : (
                <tbody>
                  {(memories.data?.data ?? []).map((memory) => (
                    <tr key={memory.id}>
                      <td className="mono">{memory.path}</td>
                      <td style={{ minWidth: 260 }}>
                        <details>
                          <summary style={{ cursor: "pointer" }}>{app.t("View content", "查看内容")}</summary>
                          <pre className="mono" style={{ whiteSpace: "pre-wrap", maxHeight: 260, overflow: "auto" }}>{memory.content || app.t("Empty", "空内容")}</pre>
                        </details>
                      </td>
                      <td>{memory.content_size_bytes} B</td>
                      <td className="mut">{new Date(memory.updated_at).toLocaleString()}</td>
                    </tr>
                  ))}
                  {(memories.data?.data.length ?? 0) === 0 && (
                    <tr><td colSpan={4} className="mut">{app.t("This store has no memory entries yet.", "该记忆库还没有内容。")}</td></tr>
                  )}
                </tbody>
              )}
            </table>
          </Card>
        )}

        {tab === "history" && versions.error instanceof Error && (
          <QueryError message={versions.error.message} retry={() => void versions.refetch()} />
        )}
        {tab === "history" && !versions.error && (
          <Card style={{ padding: 0, overflow: "auto" }}>
            <table className="table">
              <thead><tr><th>{app.t("Version", "版本")}</th><th>{app.t("Path", "路径")}</th><th>{app.t("Operation", "操作")}</th><th>{app.t("Created", "创建时间")}</th><th /></tr></thead>
              {versions.isLoading ? <SkeletonRows rows={4} cols={5} /> : (
                <tbody>
                  {(versions.data?.data ?? []).map((version) => (
                    <tr key={version.id}>
                      <td className="mono">{version.id}</td>
                      <td className="mono">{version.path}</td>
                      <td><Pill tone={version.operation === "deleted" ? "neutral" : "agent"}>{version.operation}</Pill></td>
                      <td className="mut">{new Date(version.created_at).toLocaleString()}</td>
                      <td style={{ textAlign: "right" }}>{version.redacted_at ? <Pill tone="neutral">{app.t("Redacted", "已遮盖")}</Pill> : <Button variant="danger" disabled={redact.isPending} onClick={async () => {
                        const approved = await confirm({ title: app.t("Redact this version?", "遮盖该版本？"), body: app.t("Its historical content will be permanently removed. The audit record remains.", "历史内容将被永久移除，但审计记录会保留。"), confirmLabel: app.t("Redact", "遮盖"), danger: true });
                        if (approved) redact.mutate(version.id);
                      }}>{app.t("Redact", "遮盖")}</Button>}</td>
                    </tr>
                  ))}
                  {(versions.data?.data.length ?? 0) === 0 && <tr><td colSpan={5} className="mut">{app.t("No memory versions yet.", "暂无记忆版本。")}</td></tr>}
                </tbody>
              )}
            </table>
          </Card>
        )}
      </div>
    </Drawer>
  );
}
