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
  Modal,
  Pill,
  Segmented,
  SkeletonRows,
  TextAreaField,
  TextField,
  useConfirm,
  useToast,
} from "../ui";
import DreamList from "./DreamList";
import DreamPolicyPanel from "./DreamPolicyPanel";

type Tab = "content" | "history" | "dreams" | "automation";

interface MemoryTreeRow {
  key: string;
  path: string;
  depth: number;
  directory: boolean;
  memory?: MemoryEntry;
}

export function memoryTreeRows(memories: readonly MemoryEntry[]): MemoryTreeRow[] {
  const directories = new Set<string>();
  for (const memory of memories) {
    const parts = memory.path.split("/").filter(Boolean);
    for (let index = 1; index < parts.length; index += 1) {
      directories.add(parts.slice(0, index).join("/"));
    }
  }
  const rows: MemoryTreeRow[] = [
    ...Array.from(directories, (path) => ({
      key: `dir:${path}`,
      path,
      depth: path.split("/").length - 1,
      directory: true,
    })),
    ...memories.map((memory) => ({
      key: memory.id,
      path: memory.path.replace(/^\/+/, ""),
      depth: Math.max(0, memory.path.split("/").filter(Boolean).length - 1),
      directory: false,
      memory,
    })),
  ];
  return rows.sort((left, right) => {
    const leftPath = left.path.split("/");
    const rightPath = right.path.split("/");
    const shared = Math.min(leftPath.length, rightPath.length);
    for (let index = 0; index < shared; index += 1) {
      const comparison = leftPath[index].localeCompare(rightPath[index]);
      if (comparison !== 0) return comparison;
    }
    return leftPath.length - rightPath.length;
  });
}

function MemoryEditor({
  base,
  memory,
  onClose,
  onSaved,
}: {
  base: string;
  memory?: MemoryEntry;
  onClose: () => void;
  onSaved: () => void;
}) {
  const app = useApp();
  const [path, setPath] = useState(memory?.path.replace(/^\/+/, "") ?? "notes/new.md");
  const [content, setContent] = useState(memory?.content ?? "");
  const save = useMutation({
    mutationFn: () => {
      const durablePath = `/${path.trim().replace(/^\/+/, "")}`;
      return memory
        ? api.post<MemoryEntry>(`${base}/memories/${encodeURIComponent(memory.id)}`, {
          path: durablePath,
          content,
          precondition: { content_sha256: memory.content_sha256 },
        })
        : api.post<MemoryEntry>(`${base}/memories`, { path: durablePath, content });
    },
    onSuccess: onSaved,
  });
  return (
    <Modal
      title={memory ? app.t("Edit memory", "编辑记忆") : app.t("New memory", "新建记忆")}
      onClose={onClose}
      width="min(760px, 94vw)"
      footer={<><Button onClick={onClose}>{app.t("Cancel", "取消")}</Button><Button variant="primary" disabled={!path.trim() || save.isPending} onClick={() => save.mutate()}>{app.t("Save", "保存")}</Button></>}
    >
      <TextField label={app.t("Path", "路径")} mono value={path} onChange={(event) => setPath(event.target.value)} placeholder="customers/acme/preferences.md" />
      <span className="mut">{app.t("Slash-separated paths are rendered as virtual folders. Awaken adds the required leading slash when saving.", "以 / 分隔的路径会显示为虚拟目录；保存时 Awaken 会自动补齐根路径斜杠。")}</span>
      <TextAreaField label={app.t("Content", "内容")} mono rows={16} value={content} onChange={(event) => setContent(event.target.value)} />
      {save.error instanceof Error && <div className="err">{save.error.message}</div>}
    </Modal>
  );
}

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
  const [editing, setEditing] = useState<MemoryEntry | "new" | null>(null);
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
  const remove = useMutation({
    mutationFn: (memoryId: string) => api.del(`${base}/memories/${encodeURIComponent(memoryId)}`),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["memory-store-entries", store.id] });
      void qc.invalidateQueries({ queryKey: ["memory-store-versions", store.id] });
      toast.ok(app.t("Memory deleted.", "记忆已删除。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const refreshContent = () => {
    setEditing(null);
    void qc.invalidateQueries({ queryKey: ["memory-store-entries", store.id] });
    void qc.invalidateQueries({ queryKey: ["memory-store-versions", store.id] });
  };

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
            { value: "dreams", label: "Dreams" },
            { value: "automation", label: app.t("Automation", "自动化") },
          ]}
        />

        {tab === "content" && memories.error instanceof Error && (
          <QueryError message={memories.error.message} retry={() => void memories.refetch()} />
        )}
        {tab === "content" && !memories.error && (
          <Card style={{ padding: 0, overflow: "hidden" }}>
            <div className="row" style={{ justifyContent: "space-between", padding: "10px 12px", borderBottom: "1px solid var(--line)" }}>
              <span className="mut">{app.t("ID is stable and machine-owned; name is the human label. Memory paths form virtual folders.", "ID 是稳定的机器标识，名称用于人类识别；记忆路径组成虚拟目录。")}</span>
              <Button variant="primary" onClick={() => setEditing("new")}>+ {app.t("New memory", "新建记忆")}</Button>
            </div>
            <table className="table">
              <thead><tr><th>{app.t("Path", "路径")}</th><th>{app.t("Content", "内容")}</th><th>{app.t("Size", "大小")}</th><th>{app.t("Updated", "更新时间")}</th><th /></tr></thead>
              {memories.isLoading ? <SkeletonRows rows={4} cols={5} /> : (
                <tbody>
                  {memoryTreeRows(memories.data?.data ?? []).map((row) => row.directory ? (
                    <tr key={row.key}>
                      <td colSpan={5} className="mono mut" style={{ paddingLeft: 12 + row.depth * 18 }}>▾ {row.path.split("/").at(-1)}/</td>
                    </tr>
                  ) : row.memory && (
                    <tr key={row.key}>
                      <td className="mono" style={{ paddingLeft: 12 + row.depth * 18 }}>└ {row.path.split("/").at(-1)}</td>
                      <td style={{ minWidth: 220, maxWidth: 360 }}>
                        <details>
                          <summary style={{ cursor: "pointer" }}>{app.t("View content", "查看内容")}</summary>
                          <pre className="mono" style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere", maxHeight: 260, overflowY: "auto", overflowX: "hidden" }}>{row.memory.content || app.t("Empty", "空内容")}</pre>
                        </details>
                      </td>
                      <td>{row.memory.content_size_bytes} B</td>
                      <td className="mut">{new Date(row.memory.updated_at).toLocaleString()}</td>
                      <td><span className="row" style={{ justifyContent: "flex-end" }}><Button onClick={() => setEditing(row.memory!)}>{app.t("Edit", "编辑")}</Button><Button variant="danger" disabled={remove.isPending} onClick={async () => {
                        const approved = await confirm({ title: app.t("Delete this memory?", "删除这条记忆？"), body: app.t("The current head is deleted and a deletion version is recorded.", "当前内容会删除，并记录一个删除版本。"), confirmLabel: app.t("Delete", "删除"), danger: true });
                        if (approved) remove.mutate(row.memory!.id);
                      }}>{app.t("Delete", "删除")}</Button></span></td>
                    </tr>
                  ))}
                  {(memories.data?.data.length ?? 0) === 0 && (
                    <tr><td colSpan={5} className="mut">{app.t("This store has no memory entries yet.", "该记忆库还没有内容。")}</td></tr>
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
        {tab === "dreams" && <DreamList storeId={store.id} />}
        {tab === "automation" && <DreamPolicyPanel storeId={store.id} />}
      </div>
      {editing && <MemoryEditor base={base} memory={editing === "new" ? undefined : editing} onClose={() => setEditing(null)} onSaved={refreshContent} />}
    </Drawer>
  );
}
