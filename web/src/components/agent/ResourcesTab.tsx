// Agent default inputs (ADR-0063): typed File/Memory/Repository identities composed
// with temporary Session attachments. Skills are configured in Integrations and
// outputs belong to the Environment, so neither appears in this input editor.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { Button, EmptyState, SelectField, TextField } from "../ui";
import { useToast } from "../ui/Toast";
import { api, ws } from "../../lib/api/client";
import type {
  AgentInputConfig,
  InputBinding,
  InputResourceKind,
  MemoryStore,
  Page,
  ResourceAccess,
} from "../../lib/api/types";
import { useApp } from "../../lib/app-state";

// One editable row = a binding plus a client-only label (a file's filename, shown after
// upload since the binding stores only the opaque blob id).
type Row = InputBinding & { label?: string };

const DEFAULT_MOUNT: Record<InputResourceKind, string> = {
  memory_store: "/mnt/memory",
  file: "/mnt/files/data.txt",
  repository: "/mnt/repo",
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
  const existing = useQuery({
    queryKey: ["agent-resources", agentId],
    queryFn: () => api.get<AgentInputConfig>(`/v1/config/agents/${agentId}/resources`),
    retry: false,
  });

  const [rows, setRows] = useState<Row[]>([]);
  const [version, setVersion] = useState(0);
  useEffect(() => {
    if (existing.data) {
      setRows(existing.data.inputs);
      setVersion(existing.data.revision);
    }
  }, [existing.data]);

  const storeList = stores.data?.data ?? [];
  const setRow = (i: number, patch: Partial<Row>) =>
    setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const removeRow = (i: number) => setRows((rs) => rs.filter((_, j) => j !== i));
  const addRow = (kind: InputResourceKind) =>
    setRows((rs) => [
      ...rs,
      {
        binding_id: `input-${crypto.randomUUID()}`,
        target: {
          kind,
          id: kind === "memory_store" ? (storeList[0]?.id ?? "") : "",
        },
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
      setRow(i, { target: { kind: "file", id: meta.id }, label: file.name, mount_path: `/mnt/files/${file.name}` });
      toast.ok(app.t("File uploaded.", "文件已上传。"));
    } catch (err) {
      toast.err(err instanceof Error ? err.message : "upload failed");
    }
  };

  const save = useMutation({
    mutationFn: () =>
      api.put<AgentInputConfig>(`/v1/config/agents/${agentId}/resources`, {
        agent_id: agentId,
        // Strip the client-only `label` before persisting.
        inputs: rows.map(({ label: _label, ...b }) => b),
        revision: version + 1,
      }),
    onSuccess: (r) => {
      setVersion(r.revision);
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
            "Bind memory stores, immutable files, and managed repositories as Agent defaults. Add Skills under Integrations; outputs are configured by the Environment.",
            "把记忆库、不可变文件和平台管理的代码仓绑定为 Agent 默认输入。技能在集成中配置；输出由环境配置。",
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
            value={r.target.kind}
            onChange={(e) => {
              const kind = e.target.value as InputResourceKind;
              setRow(i, { target: { kind, id: "" }, mount_path: DEFAULT_MOUNT[kind], label: undefined });
            }}
          >
            <option value="memory_store">memory_store</option>
            <option value="file">file</option>
            <option value="repository">repository</option>
          </SelectField>

          {r.target.kind === "memory_store" && (
            <SelectField label={app.t("Store", "记忆库")} value={r.target.id} onChange={(e) => setRow(i, { target: { kind: "memory_store", id: e.target.value } })}>
              {storeList.map((s) => (
                <option key={s.id} value={s.id}>
                  {s.name || s.id}
                </option>
              ))}
            </SelectField>
          )}
          {r.target.kind === "file" && (
            <div className="field">
              <label>{app.t("File", "文件")}</label>
              <Button style={{ height: 26 }} onClick={() => pickFile(i)}>
                {r.label || r.target.id ? (r.label ?? r.target.id) : app.t("Upload…", "上传…")}
              </Button>
            </div>
          )}
          {r.target.kind === "repository" && (
            <TextField
              label={app.t("Repository id", "代码仓 ID")}
              mono
              placeholder="repo-…"
              value={r.target.id}
              onChange={(e) => setRow(i, { target: { kind: "repository", id: e.target.value } })}
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
        <Button onClick={() => addRow("repository")}>+ {app.t("connect a repo", "连接仓库")}</Button>
        <Button variant="primary" disabled={save.isPending} onClick={() => save.mutate()}>
          {app.t("Save resources", "保存资源")}
        </Button>
      </div>
    </>
  );
}
