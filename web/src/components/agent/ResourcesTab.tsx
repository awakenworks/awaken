// Agent default inputs (ADR-0063): typed File/Memory/Repository identities composed
// with temporary Session attachments. Skills are configured under Build / Skills & MCP and
// outputs belong to the Environment, so neither appears in this input editor.

import { useQuery } from "@tanstack/react-query";
import { useRef, useState } from "react";
import { useNavigate } from "react-router";
import { Button, EmptyState, SelectField, TextAreaField, TextField } from "../ui";
import { useToast } from "../ui/Toast";
import { api, ws } from "../../lib/api/client";
import type {
  InputBinding,
  InputResourceKind,
  MemoryStore,
  Page,
  ResourceInputDefaultMounts,
  ResourceAccess,
} from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import {
  createDefaultResourceBinding,
  switchResourceBindingKind,
} from "../../lib/resource-input-defaults";

// One editable row = a binding plus a client-only label (a file's filename, shown after
// upload since the binding stores only the opaque blob id).
type Row = InputBinding & { label?: string };

export default function ResourcesTab({
  inputs,
  defaultMounts,
  onChange,
}: {
  inputs: InputBinding[];
  defaultMounts?: ResourceInputDefaultMounts;
  onChange: (inputs: InputBinding[]) => void;
}) {
  const app = useApp();
  const navigate = useNavigate();
  const toast = useToast();
  const fileInput = useRef<HTMLInputElement>(null);
  const uploadingRow = useRef<number | null>(null);

  const stores = useQuery({
    queryKey: ["memory-stores"],
    queryFn: () => api.get<Page<MemoryStore>>(ws("/v1/memory_stores")),
  });
  const storeList = stores.data?.data ?? [];
  const rows = inputs as Row[];
  const [fileLabels, setFileLabels] = useState<Record<string, string>>({});
  const setRow = (i: number, patch: Partial<Row>) =>
    onChange(rows.map((r, j) => (j === i ? { ...r, ...patch } : r)).map(({ label: _label, ...binding }) => binding));
  const removeRow = (i: number) => onChange(rows.filter((_, j) => j !== i));
  const addRow = (kind: InputResourceKind) => {
    const candidate = createDefaultResourceBinding(
      defaultMounts,
      kind,
      `input-${crypto.randomUUID()}`,
      kind === "memory_store" ? (storeList[0]?.id ?? "") : "",
    );
    if (candidate) onChange([...rows, candidate]);
  };

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
      const meta = await api.upload<{ id: string; filename?: string }>(ws("/v1/files"), file);
      const bindingId = rows[i]?.binding_id;
      if (bindingId) setFileLabels((current) => ({ ...current, [bindingId]: file.name }));
      setRow(i, { target: { kind: "file", id: meta.id }, mount_path: `/mnt/files/${file.name}` });
      toast.ok(app.t("File uploaded.", "文件已上传。"));
    } catch (err) {
      toast.err(err instanceof Error ? err.message : "upload failed");
    }
  };

  return (
    <>
      <input ref={fileInput} type="file" style={{ display: "none" }} onChange={onFileChosen} />
      <div className="field">
        <label>{app.t("Resources mounted for this agent", "本 agent 挂载的资源")}</label>
        <span className="mut">
          {app.t(
            "Bind memory stores, immutable files, and managed repositories as Agent defaults. Add Skills under Build / Skills & MCP; outputs are configured by the Environment.",
            "把记忆库、不可变文件和平台管理的代码仓绑定为 Agent 默认输入。技能在构建 / Skills 与 MCP 中配置；输出由 Environment 配置。",
          )}
        </span>
      </div>

      {!defaultMounts && (
        <div className="banner warn">
          <span>⚠</span>
          <span>{app.t(
            "Resource mount defaults are unavailable. Adding resources and changing kinds are disabled.",
            "资源挂载默认值不可用；已禁用新增资源和切换类型。",
          )}</span>
        </div>
      )}

      {rows.length === 0 && (
        <EmptyState
          title={app.t("No resources bound yet.", "尚未绑定资源。")}
          hint={app.t("Add one below to mount it in every session this agent runs.", "在下方添加,让它在本 agent 的每个会话中挂载。")}
        />
      )}

      {rows.map((r, i) => (
        <div key={r.binding_id} className="stack" style={{ gap: 6, padding: "10px 0", borderBottom: "1px solid var(--line)" }}>
        <div className="row" style={{ alignItems: "flex-end", gap: 8 }}>
          <SelectField
            label={app.t("Kind", "类型")}
            value={r.target.kind}
            disabled={!defaultMounts}
            onChange={(e) => {
              const kind = e.target.value as InputResourceKind;
              const candidate = switchResourceBindingKind(r, defaultMounts, kind);
              if (candidate) {
                setRow(i, {
                  target: candidate.target,
                  mount_path: candidate.mount_path,
                  access: candidate.access,
                  label: undefined,
                });
              }
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
                {fileLabels[r.binding_id] || r.target.id || app.t("Upload…", "上传…")}
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
        <TextAreaField
          label={app.t("Instructions for this resource (optional)", "该资源的注入提示词（可选）")}
          hint={app.t("Tell the Agent what this resource contains and how to use it. This text is injected with the mounted resource at session preparation.", "说明该资源包含什么、应如何使用；会话准备时会随挂载资源一起注入。")}
          rows={2}
          value={r.instructions ?? ""}
          onChange={(event) => setRow(i, { instructions: event.target.value || undefined })}
        />
        </div>
      ))}

      <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
        <Button
          disabled={!defaultMounts || stores.isLoading || storeList.length === 0}
          title={storeList.length === 0 ? app.t("Create a Memory Store first", "请先创建记忆库") : undefined}
          onClick={() => addRow("memory_store")}
        >+ {app.t("bind a store", "绑定记忆库")}</Button>
        <Button disabled={!defaultMounts} onClick={() => addRow("file")}>+ {app.t("attach a file", "附加文件")}</Button>
        <Button disabled={!defaultMounts} onClick={() => addRow("repository")}>+ {app.t("connect a repo", "连接仓库")}</Button>
        {!stores.isLoading && storeList.length === 0 && !stores.error && (
          <Button variant="ghost" onClick={() => navigate(`/w/${app.workspaceId}/memory`)}>
            {app.t("Create a Memory Store →", "创建记忆库 →")}
          </Button>
        )}
        <span className="mut">{app.t("Resource changes are part of this Agent draft and are used by Try immediately.", "资源改动属于当前 Agent 草稿，试运行会立即使用。")}</span>
      </div>
      {stores.error instanceof Error && (
        <div className="err">
          {stores.error.message}{" "}
          <Button variant="ghost" onClick={() => void stores.refetch()}>{app.t("Try again", "重试")}</Button>
        </div>
      )}
    </>
  );
}
