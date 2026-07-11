// Agent resources (ADR-0038): bind the resources an agent mounts in every session it
// runs — memory stores (mutable, write-back), files (immutable blobs), GitHub repos
// (host-side clone), and skills (bundled instructions). Each binding realizes two ways:
// a sandbox mount the runtime stages at session-create, and a prompt fragment appended
// to the agent's system prompt. This closes the loop — a resource is no longer an orphan;
// it becomes something the agent actually reads/writes. Authored via
// PUT /v1/config/agents/:id/resources; the config service shares the admin resource store.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { Button, EmptyState, SelectField, TextField } from "../ui";
import { useToast } from "../ui/Toast";
import { api, ws } from "../../lib/api/client";
import type {
  AgentResourceConfig,
  MemoryStore,
  Page,
  ResourceAccess,
  ResourceBinding,
  ResourceKind,
  Skill,
} from "../../lib/api/types";
import { useApp } from "../../lib/app-state";

// One editable row = a binding plus a client-only label (a file's filename, shown after
// upload since the binding stores only the opaque blob id).
type Row = ResourceBinding & { label?: string };

const DEFAULT_MOUNT: Record<ResourceKind, string> = {
  memory_store: "/mnt/memory",
  file: "/mnt/files/data.txt",
  github_repository: "/mnt/repo",
  skill: "/mnt/skills/skill",
  outputs: "/mnt/session/outputs",
};

export default function ResourcesTab({ agentId }: { agentId: string }) {
  const app = useApp();
  const qc = useQueryClient();
  const toast = useToast();
  const fileInput = useRef<HTMLInputElement>(null);
  const uploadingRow = useRef<number | null>(null);

  const stores = useQuery({
    queryKey: ["memory-stores"],
    queryFn: () => api.get<Page<MemoryStore>>(ws("/v1/memory_stores")),
  });
  const skills = useQuery({
    queryKey: ["skills"],
    queryFn: () => api.get<Page<Skill>>(ws("/v1/skills")),
  });
  const existing = useQuery({
    queryKey: ["agent-resources", agentId],
    queryFn: () => api.get<AgentResourceConfig>(`/v1/config/agents/${agentId}/resources`),
    retry: false,
  });

  const [rows, setRows] = useState<Row[]>([]);
  const [version, setVersion] = useState(0);
  useEffect(() => {
    if (existing.data) {
      setRows(existing.data.resources);
      setVersion(existing.data.version);
    }
  }, [existing.data]);

  const storeList = stores.data?.data ?? [];
  const skillList = skills.data?.data ?? [];

  const setRow = (i: number, patch: Partial<Row>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const removeRow = (i: number) => setRows((rs) => rs.filter((_, j) => j !== i));
  const addRow = (kind: ResourceKind) =>
    setRows((rs) => [
      ...rs,
      {
        kind,
        resource_id:
          kind === "memory_store" ? (storeList[0]?.id ?? "") : kind === "skill" ? (skillList[0]?.id ?? "") : "",
        mount_path: DEFAULT_MOUNT[kind],
        access: kind === "memory_store" ? "read_write" : "read_only",
      },
    ]);

  // A file binding stores a blob id, so uploading is a two-step: pick a file → POST it to
  // the Files API → stamp the returned id (and its filename, for display) onto the row.
  const pickFile = (i: number) => {
    uploadingRow.current = i;
    fileInput.current?.click();
  };
  const onFileChosen = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    const i = uploadingRow.current;
    e.target.value = "";
    if (!file || i == null) return;
    try {
      const meta = await api.upload<{ id: string; filename?: string }>(ws("/v1/files"), file, { purpose: "agent" });
      setRow(i, { resource_id: meta.id, label: file.name, mount_path: `/mnt/files/${file.name}` });
      toast.ok(app.t("File uploaded.", "文件已上传。"));
    } catch (err) {
      toast.err(err instanceof Error ? err.message : "upload failed");
    }
  };

  const save = useMutation({
    mutationFn: () =>
      api.put<AgentResourceConfig>(`/v1/config/agents/${agentId}/resources`, {
        agent_id: agentId,
        // Strip the client-only `label` before persisting.
        resources: rows.map(({ label: _label, ...b }) => b),
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
      <input ref={fileInput} type="file" style={{ display: "none" }} onChange={onFileChosen} />
      <div className="field">
        <label>{app.t("Resources mounted for this agent", "本 agent 挂载的资源")}</label>
        <span className="mut">
          {app.t(
            "Bind memory stores, files, GitHub repos, and skills to sandbox paths — the agent reads/writes them and each mount is described in its system prompt. Memory persists across sessions.",
            "把记忆库、文件、GitHub 仓库、技能挂到 sandbox 路径——agent 读写它们,每个挂载都写进系统提示;记忆跨会话保留。",
          )}
        </span>
      </div>

      {rows.length === 0 && (
        <EmptyState
          title={app.t("No resources bound yet.", "尚未绑定资源。")}
          hint={app.t("Add one below to mount it in every session this agent runs.", "在下方添加,让它在本 agent 的每个会话中挂载。")}
        />
      )}

      {rows.map((r, i) => (
        <div className="row" key={i} style={{ alignItems: "flex-end", gap: 8 }}>
          <SelectField
            label={app.t("Kind", "类型")}
            value={r.kind}
            onChange={(e) => {
              const kind = e.target.value as ResourceKind;
              setRow(i, { kind, resource_id: "", mount_path: DEFAULT_MOUNT[kind], label: undefined });
            }}
          >
            <option value="memory_store">memory_store</option>
            <option value="file">file</option>
            <option value="github_repository">github_repository</option>
            <option value="skill">skill</option>
          </SelectField>

          {r.kind === "memory_store" && (
            <SelectField label={app.t("Store", "记忆库")} value={r.resource_id ?? ""} onChange={(e) => setRow(i, { resource_id: e.target.value })}>
              {storeList.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.name || s.id}
                </option>
              ))}
            </SelectField>
          )}
          {r.kind === "skill" && (
            <SelectField label={app.t("Skill", "技能")} value={r.resource_id ?? ""} onChange={(e) => setRow(i, { resource_id: e.target.value })}>
              {skillList.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.display_name || s.name || s.id}
                </option>
              ))}
            </SelectField>
          )}
          {r.kind === "file" && (
            <div className="field">
              <label>{app.t("File", "文件")}</label>
              <Button style={{ height: 26 }} onClick={() => pickFile(i)}>
                {r.label || r.resource_id ? (r.label ?? r.resource_id) : app.t("Upload…", "上传…")}
              </Button>
            </div>
          )}
          {r.kind === "github_repository" && (
            <TextField
              label={app.t("Repo URL", "仓库地址")}
              mono
              placeholder="https://github.com/owner/repo.git"
              value={r.resource_id ?? ""}
              onChange={(e) => setRow(i, { resource_id: e.target.value })}
            />
          )}

          <TextField label={app.t("Mount path", "挂载路径")} mono placeholder="/mnt/…" value={r.mount_path} onChange={(e) => setRow(i, { mount_path: e.target.value })} />
          <SelectField label={app.t("Access", "访问")} value={r.access} onChange={(e) => setRow(i, { access: e.target.value as ResourceAccess })}>
            <option value="read_write">read_write</option>
            <option value="read_only">read_only</option>
          </SelectField>
          <Button variant="ghost" style={{ height: 26 }} onClick={() => removeRow(i)}>
            ✕
          </Button>
        </div>
      ))}

      <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
        <Button onClick={() => addRow("memory_store")}>+ {app.t("bind a store", "绑定记忆库")}</Button>
        <Button onClick={() => addRow("file")}>+ {app.t("attach a file", "附加文件")}</Button>
        <Button onClick={() => addRow("github_repository")}>+ {app.t("connect a repo", "连接仓库")}</Button>
        <Button onClick={() => addRow("skill")}>+ {app.t("add a skill", "添加技能")}</Button>
        <Button variant="primary" disabled={save.isPending} onClick={() => save.mutate()}>
          {app.t("Save resources", "保存资源")}
        </Button>
      </div>
    </>
  );
}
