// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: Authoring form for one MCP server definition. It submits a whole configuration for the
// @formal reason: server to validate and never writes partial state or credential material. Classification
// @formal reason: only, not formal evidence.
import { useState } from "react";
import { Button } from "@awaken/ui";
import { Modal, TextField, TextAreaField, useConfirm } from "../ui";
import { useUnsavedGuard } from "../../lib/useUnsavedGuard";
import { useApp } from "../../lib/app-state";
import type { McpDefinition } from "../../lib/api/mcp-types";

export default function McpServerForm({ initial, pending, error, onSave, onClose, onReload }: {
  initial?: McpDefinition; pending: boolean; error?: Error | null;
  onReload: () => void; onSave: (definition: McpDefinition) => void; onClose: () => void;
}) {
  const app = useApp(); const confirm = useConfirm();
  const [draft, setDraft] = useState<McpDefinition>(initial ?? {
    id: "", name: "", description: "", icon: null, transport: { type: "url", url: "" }, prompts_as_skills: false,
  });
  const [httpUrl, setHttpUrl] = useState(initial?.transport.type === "url" ? initial.transport.url : "");
  const [command, setCommand] = useState(initial?.transport.type === "sandbox_stdio" ? initial.transport.command : "");
  const [changed, setChanged] = useState(false);
  useUnsavedGuard(changed && !pending, app.t("Leave without saving MCP server changes?", "离开并放弃未保存的 MCP 服务器修改？"));
  const [args, setArgs] = useState(initial?.transport.type === "sandbox_stdio" ? (initial.transport.args ?? []).join("\n") : "");
  const patch = (value: Partial<McpDefinition>) => { setChanged(true); setDraft((d) => ({ ...d, ...value })); };
  const close = async () => {
    if (pending) return;
    if (!changed || await confirm({ title: app.t("Discard unsaved server changes?", "放弃未保存的服务器修改？"), confirmLabel: app.t("Discard", "放弃") })) onClose();
  };
  return <Modal title={app.t(initial ? "Edit MCP server" : "Add MCP server", initial ? "编辑 MCP 服务器" : "添加 MCP 服务器")} onClose={() => void close()}>
    <form className="ui-stack" onSubmit={(event) => { event.preventDefault(); if (!pending) onSave(draft); }}>
      <fieldset disabled={pending} className="mcp-fields ui-stack">
      <TextField label={app.t("Server ID", "服务器 ID")} required maxLength={80} pattern="(?:[A-Za-z0-9_]|-)+" title={app.t("Use letters, numbers, hyphens or underscores", "使用英文字母、数字、短横线或下划线")} value={draft.id} disabled={Boolean(initial)} onChange={(e) => patch({ id: e.target.value })} />
      <TextField label={app.t("Display name", "显示名称")} required maxLength={256} value={draft.name} onChange={(e) => patch({ name: e.target.value })} />
      <TextAreaField label={app.t("Description", "说明")} maxLength={4096} value={draft.description} onChange={(e) => patch({ description: e.target.value })} />
      <TextField label={app.t("Icon URL · optional", "图标 URL · 可选")} type="url" pattern="https://.*" value={draft.icon ?? ""} placeholder="https://…" onChange={(e) => patch({ icon: e.target.value || null })} />
      <label className="ui-field"><span>{app.t("Transport", "传输方式")}</span><select className="ui-input" aria-label={app.t("Transport", "传输方式")} value={draft.transport.type}
        onChange={(e) => patch({ transport: e.target.value === "url" ? { type: "url", url: httpUrl } : { type: "sandbox_stdio", command, args: args.split("\n").filter(Boolean) } })}>
        <option value="url">HTTP</option><option value="sandbox_stdio">Sandbox stdio</option>
      </select></label>
      {draft.transport.type === "url" ? <TextField label="MCP URL" required type="url" pattern="https?://.*" value={draft.transport.url} onChange={(e) => { setHttpUrl(e.target.value); patch({ transport: { type: "url", url: e.target.value } }); }} />
        : <><TextField label={app.t("Sandbox command", "Sandbox 命令")} required value={draft.transport.command} onChange={(e) => { setCommand(e.target.value); patch({ transport: { type: "sandbox_stdio", command: e.target.value, args: args.split("\n").filter(Boolean) } }); }} />
          <TextAreaField label={app.t("Arguments · one per line", "参数 · 每行一个")} value={args} onChange={(e) => { setArgs(e.target.value); if (draft.transport.type === "sandbox_stdio") patch({ transport: { ...draft.transport, args: e.target.value.split("\n").filter(Boolean) } }); }} />
          <p className="hint">{app.t("The command must exist inside the selected Session environment. Uploaded scripts use their /mnt/session/uploads path.", "命令必须存在于所选 Session 环境中；上传脚本使用 /mnt/session/uploads 下的路径。")}</p></>}
      <label><input type="checkbox" checked={draft.prompts_as_skills ?? false} onChange={(e) => patch({ prompts_as_skills: e.target.checked })} /> {app.t("Expose MCP prompts as Skills", "将 MCP Prompt 暴露为 Skill")}</label>
      {error && <div role="alert" className="err">{error.message}<Button type="button" onClick={onReload}>{app.t("Reload configuration", "重新加载配置")}</Button></div>}
      <Button variant="primary" type="submit" disabled={pending || !draft.id.trim() || !draft.name.trim()}>{app.t("Save server", "保存服务器")}</Button>
      </fieldset>
    </form>
  </Modal>;
}
