// Workspace · Memory stores: persistent memory that survives
// across sessions (Managed Agents `/v1/memory_stores`). A session mounts one
// via a `resources[]` entry.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { useSearchParams } from "react-router";
import { api, ws } from "../lib/api/client";
import type { MemoryStore, Page } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Button, Card, EmptyState, Modal, Pill, Segmented, SkeletonRows, TechnicalId, TextField, useConfirm, useToast } from "../components/ui";
import MemoryStoreDrawer from "../components/memory/MemoryStoreDrawer";
import DreamList from "../components/memory/DreamList";
import { entityDisplayName, identifierLabel } from "../lib/presentation";

function CreateModal({ onClose }: { onClose: () => void }) {
  const app = useApp();
  const toast = useToast();
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const create = useMutation({
    mutationFn: () =>
      api.post<MemoryStore>(ws("/v1/memory_stores"), {
        name: name || "memory-store",
        ...(description ? { description } : {}),
      }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["memory-stores"] });
      toast.ok(app.t("Memory store created.", "记忆库已创建。"));
      onClose();
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  return (
    <Modal title={app.t("New memory store", "新建记忆库")} onClose={onClose}>
        <TextField label={app.t("Name", "名称")} value={name} onChange={(e) => setName(e.target.value)} placeholder="project-memory" />
        <TextField
          label={app.t("Description", "描述")}
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          placeholder={app.t("What this store remembers", "这个记忆库保存什么")}
        />
        {create.error instanceof Error && <div className="err">{create.error.message}</div>}
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <Button onClick={onClose}>
            {app.t("Cancel", "取消")}
          </Button>
          <Button variant="primary" disabled={create.isPending} onClick={() => create.mutate()}>
            {app.t("Create", "创建")}
          </Button>
        </div>
    </Modal>
  );
}

export default function MemorySurface() {
  const app = useApp();
  const qc = useQueryClient();
  const confirm = useConfirm();
  const toast = useToast();
  const [creating, setCreating] = useState(false);
  const [selected, setSelected] = useState<MemoryStore | null>(null);
  const [surface, setSurface] = useState<"stores" | "dreams">("stores");
  const [searchParams, setSearchParams] = useSearchParams();
  const stores = useQuery({
    queryKey: ["memory-stores"],
    queryFn: () => api.get<Page<MemoryStore>>(ws("/v1/memory_stores")),
    refetchInterval: 30_000,
  });
  const archive = useMutation({
    mutationFn: (id: string) => api.post(ws(`/v1/memory_stores/${id}/archive`)),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["memory-stores"] });
      toast.ok(app.t("Memory store archived.", "记忆库已归档。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.del(ws(`/v1/memory_stores/${id}`)),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["memory-stores"] });
      toast.ok(app.t("Memory store scheduled for deletion.", "记忆库已进入删除流程。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const rows = stores.data?.data ?? [];
  useEffect(() => {
    const requested = searchParams.get("store");
    if (requested && rows.length) {
      const store = rows.find((candidate) => candidate.id === requested);
      if (store) setSelected(store);
    }
  }, [rows, searchParams]);

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="row">
          <Segmented value={surface} onChange={setSurface} options={[
            { value: "stores", label: app.t("Stores", "记忆库") },
            { value: "dreams", label: "Dreams" },
          ]} />
          <span className="mut">
          {app.t(
            surface === "stores" ? "Editable content shared across selected Agent Sessions." : "Review the source, evidence, status, and output of each consolidation run.",
            surface === "stores" ? "可供指定 Agent 会话共享和编辑的持久内容。" : "检查每次整理任务的来源、证据、状态和输出。",
          )}
          </span>
        </span>
        {surface === "stores" && <Button variant="primary" onClick={() => setCreating(true)}>
          + {app.t("New memory store", "新建记忆库")}
        </Button>}
      </div>
      {surface === "dreams" ? <DreamList /> : stores.error instanceof Error ? (
        <Card>
          <EmptyState
            title={app.t("Memory stores could not be loaded", "记忆库加载失败")}
            hint={stores.error.message}
            action={<Button onClick={() => void stores.refetch()}>{app.t("Try again", "重试")}</Button>}
          />
        </Card>
      ) : <Card className="responsive-table-card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Memory store", "记忆库")}</th>
              <th>{app.t("Description", "描述")}</th>
              <th />
            </tr>
          </thead>
          {stores.isLoading ? <SkeletonRows rows={4} cols={3} /> : <tbody>
            {rows.map((s) => (
              <tr key={s.id}>
                <td data-label={app.t("Memory store", "记忆库")}><strong>{entityDisplayName(s.name, identifierLabel(s.id))}</strong><TechnicalId value={s.id} /></td>
                <td data-label={app.t("Description", "描述")} className="mut">{s.description ?? "—"}</td>
                <td className="responsive-table-actions" style={{ textAlign: "right" }}>
                  <div className="row" style={{ justifyContent: "flex-end" }}>
                    <Button variant="ghost" style={{ height: 22 }} onClick={() => setSelected(s)}>
                      {app.t("Open", "打开")}
                    </Button>
                    {s.archived_at ? (
                      <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>
                    ) : (
                      <Button
                        variant="ghost"
                        style={{ height: 22 }}
                        disabled={archive.isPending}
                        onClick={async () => {
                          const approved = await confirm({
                            title: app.t("Archive this memory store?", "归档该记忆库？"),
                            body: app.t("New sessions will no longer be able to bind it. Existing governed sessions fail closed if they try to use it again.", "新会话将无法再绑定它；已有受管会话再次使用时会安全失败。"),
                            confirmLabel: app.t("Archive", "归档"),
                          });
                          if (approved) archive.mutate(s.id);
                        }}
                      >
                        {app.t("Archive", "归档")}
                      </Button>
                    )}
                    <Button
                      variant="danger"
                      style={{ height: 22 }}
                      disabled={remove.isPending}
                      onClick={async () => {
                        const approved = await confirm({
                          title: app.t("Delete this memory store?", "删除该记忆库？"),
                          body: app.t("The store becomes unavailable immediately and its data is purged according to the retention policy. This cannot be undone.", "记忆库会立即不可用，并按保留策略清理数据。此操作无法撤销。"),
                          confirmLabel: app.t("Delete", "删除"),
                          danger: true,
                        });
                        if (approved) remove.mutate(s.id);
                      }}
                    >
                      {app.t("Delete", "删除")}
                    </Button>
                  </div>
                </td>
              </tr>
            ))}
            {!stores.isLoading && rows.length === 0 && (
              <tr>
                <td colSpan={3} className="mut">
                  {app.t("No Memory Stores yet. Create one for knowledge that must remain editable across Sessions.", "还没有 Memory Store。需要跨会话保留且可编辑的知识时，请创建一个。")}
                </td>
              </tr>
            )}
          </tbody>}
        </table>
      </Card>}
      {creating && <CreateModal onClose={() => setCreating(false)} />}
      {selected && <MemoryStoreDrawer store={selected} onClose={() => { setSelected(null); if (searchParams.has("store")) { const next = new URLSearchParams(searchParams); next.delete("store"); setSearchParams(next, { replace: true }); } }} />}
    </>
  );
}
