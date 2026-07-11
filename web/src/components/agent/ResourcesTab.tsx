// Agent resources (ADR-0038): bind memory stores to the agent so every session it
// runs mounts them at a sandbox path (read/write, write-back harvested) and the
// mount is described in the agent's system prompt. This closes the loop — a memory
// store you create is no longer an orphan; it becomes memory the agent actually uses.
// Authored via PUT /v1/config/agents/:id/resources; read into resource prompts +
// mounts at compile (the config service shares the admin resource store).

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { Button, EmptyState, SelectField, TextField } from "../ui";
import { useToast } from "../ui/Toast";
import { api, ws } from "../../lib/api/client";
import type {
  AgentResourceConfig,
  MemoryStore,
  Page,
  ResourceAccess,
  ResourceBinding,
} from "../../lib/api/types";
import { useApp } from "../../lib/app-state";

export default function ResourcesTab({ agentId }: { agentId: string }) {
  const app = useApp();
  const qc = useQueryClient();
  const toast = useToast();

  const stores = useQuery({
    queryKey: ["memory-stores"],
    queryFn: () => api.get<Page<MemoryStore>>(ws("/v1/memory_stores")),
  });
  const existing = useQuery({
    queryKey: ["agent-resources", agentId],
    queryFn: () => api.get<AgentResourceConfig>(`/v1/config/agents/${agentId}/resources`),
    retry: false,
  });

  // Local editable copy of the agent's memory-store bindings (other kinds are
  // preserved untouched on save so we never drop a file/repo binding).
  const [rows, setRows] = useState<ResourceBinding[]>([]);
  const [others, setOthers] = useState<ResourceBinding[]>([]);
  const [version, setVersion] = useState(0);
  useEffect(() => {
    if (existing.data) {
      setRows(existing.data.resources.filter((r) => r.kind === "memory_store"));
      setOthers(existing.data.resources.filter((r) => r.kind !== "memory_store"));
      setVersion(existing.data.version);
    }
  }, [existing.data]);

  const storeList = stores.data?.data ?? [];
  const setRow = (i: number, patch: Partial<ResourceBinding>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const addRow = () =>
    setRows((rs) => [
      ...rs,
      { kind: "memory_store", resource_id: storeList[0]?.id ?? "", mount_path: "/mnt/memory", access: "read_write" },
    ]);
  const removeRow = (i: number) => setRows((rs) => rs.filter((_, j) => j !== i));

  const save = useMutation({
    mutationFn: () =>
      api.put<AgentResourceConfig>(`/v1/config/agents/${agentId}/resources`, {
        agent_id: agentId,
        resources: [...others, ...rows],
        version: version + 1,
      }),
    onSuccess: (r) => {
      setVersion(r.version);
      toast.ok(app.t("Resources saved.", "资源已保存。"));
      void qc.invalidateQueries({ queryKey: ["agent-resources", agentId] });
    },
    onError: (e) => toast.err(e instanceof Error ? e.message : "error"),
  });

  return (
    <>
      <div className="field">
        <label>{app.t("Memory stores mounted for this agent", "本 agent 挂载的记忆库")}</label>
        <span className="mut">
          {app.t(
            "Bind a memory store to a sandbox path — the agent reads/writes it and the mount is described in its system prompt. Persisted memory survives across sessions.",
            "把记忆库挂到 sandbox 的一个路径——agent 读写它,挂载会写进系统提示;持久记忆跨会话保留。",
          )}
        </span>
      </div>
      {storeList.length === 0 ? (
        <EmptyState
          title={app.t("No memory stores yet.", "还没有记忆库。")}
          hint={app.t("Create one under Memory stores, then bind it here.", "先在「记忆库」建一个,再来这里绑定。")}
        />
      ) : (
        <>
          {rows.map((r, i) => (
            <div className="row" key={i} style={{ alignItems: "flex-end" }}>
              <SelectField
                label={app.t("Store", "记忆库")}
                value={r.resource_id ?? ""}
                onChange={(e) => setRow(i, { resource_id: e.target.value })}
              >
                {storeList.map((s) => (
                  <option key={s.id} value={s.id}>
                    {s.name || s.id}
                  </option>
                ))}
              </SelectField>
              <TextField
                label={app.t("Mount path", "挂载路径")}
                mono
                value={r.mount_path}
                onChange={(e) => setRow(i, { mount_path: e.target.value })}
              />
              <SelectField
                label={app.t("Access", "访问")}
                value={r.access}
                onChange={(e) => setRow(i, { access: e.target.value as ResourceAccess })}
              >
                <option value="read_write">read_write</option>
                <option value="read_only">read_only</option>
              </SelectField>
              <Button variant="ghost" style={{ height: 26 }} onClick={() => removeRow(i)}>
                ✕
              </Button>
            </div>
          ))}
          {rows.length === 0 && (
            <span className="mut">{app.t("No store bound yet.", "尚未绑定记忆库。")}</span>
          )}
          <div className="row">
            <Button onClick={addRow}>+ {app.t("bind a store", "绑定记忆库")}</Button>
            <Button variant="primary" disabled={save.isPending} onClick={() => save.mutate()}>
              {app.t("Save resources", "保存资源")}
            </Button>
          </div>
        </>
      )}
    </>
  );
}
