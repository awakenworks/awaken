// Project · Memory stores: workspace-scoped persistent memory that survives
// across sessions (Managed Agents `/v1/memory_stores`). A session mounts one
// via a `resources[]` entry.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api/client";
import type { MemoryStore, Page } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Button, Card, Pill, TextField } from "../components/ui";

function CreateModal({ onClose }: { onClose: () => void }) {
  const app = useApp();
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const create = useMutation({
    mutationFn: () =>
      api.post<MemoryStore>("/v1/memory_stores", {
        name: name || "memory-store",
        ...(description ? { description } : {}),
      }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["memory-stores"] });
      onClose();
    },
  });
  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>{app.t("New memory store", "新建记忆库")}</h3>
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
      </div>
    </div>
  );
}

export default function MemorySurface() {
  const app = useApp();
  const qc = useQueryClient();
  const [creating, setCreating] = useState(false);
  const stores = useQuery({
    queryKey: ["memory-stores"],
    queryFn: () => api.get<Page<MemoryStore>>("/v1/memory_stores"),
    refetchInterval: 30_000,
  });
  const archive = useMutation({
    mutationFn: (id: string) => api.post(`/v1/memory_stores/${id}/archive`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["memory-stores"] }),
  });
  const remove = useMutation({
    mutationFn: (id: string) => api.del(`/v1/memory_stores/${id}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["memory-stores"] }),
  });
  const rows = stores.data?.data ?? [];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut">
          {app.t(
            "Workspace-scoped persistent memory that survives across sessions. A session mounts one via a resources[] entry.",
            "工作区级别的持久化记忆,跨会话保留。会话通过 resources[] 条目挂载其一。",
          )}
        </span>
        <Button variant="primary" onClick={() => setCreating(true)}>
          + {app.t("New memory store", "新建记忆库")}
        </Button>
      </div>
      {stores.error instanceof Error && <div className="err">{stores.error.message}</div>}
      <Card style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Store", "记忆库")}</th>
              <th>{app.t("Name", "名称")}</th>
              <th>{app.t("Description", "描述")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((s) => (
              <tr key={s.id}>
                <td className="mono">{s.id}</td>
                <td>{s.name}</td>
                <td className="mut">{s.description ?? "—"}</td>
                <td style={{ textAlign: "right" }}>
                  <div className="row" style={{ justifyContent: "flex-end" }}>
                    {s.archived_at ? (
                      <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>
                    ) : (
                      <Button
                        variant="ghost"
                        style={{ height: 22 }}
                        disabled={archive.isPending}
                        onClick={() => archive.mutate(s.id)}
                      >
                        {app.t("Archive", "归档")}
                      </Button>
                    )}
                    <Button
                      variant="danger"
                      style={{ height: 22 }}
                      disabled={remove.isPending}
                      onClick={() => remove.mutate(s.id)}
                    >
                      {app.t("Delete", "删除")}
                    </Button>
                  </div>
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={4} className="mut">
                  {stores.isLoading ? "…" : app.t("No memory stores yet.", "还没有记忆库。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>
      {creating && <CreateModal onClose={() => setCreating(false)} />}
    </>
  );
}
