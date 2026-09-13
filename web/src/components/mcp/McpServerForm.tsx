import { useState } from "react";
import { Button } from "@awaken/ui";
import { Modal, TextField, TextAreaField } from "../ui";
import { useApp } from "../../lib/app-state";
import type { McpDefinition } from "../../lib/api/mcp-types";

export default function McpServerForm({ initial, pending, error, onSave, onClose }: {
  initial?: McpDefinition; pending: boolean; error?: Error | null;
  onSave: (definition: McpDefinition) => void; onClose: () => void;
}) {
  const app = useApp();
  const [draft, setDraft] = useState<McpDefinition>(initial ?? {
    id: "", name: "", description: "", icon: null, transport: { type: "url", url: "" }, prompts_as_skills: false,
  });
  const [args, setArgs] = useState(initial?.transport.type === "sandbox_stdio" ? (initial.transport.args ?? []).join("\n") : "");
  const patch = (value: Partial<McpDefinition>) => setDraft((d) => ({ ...d, ...value }));
  return <Modal title={app.t(initial ? "Edit MCP server" : "Add MCP server", initial ? "编辑 MCP 服务器" : "添加 MCP 服务器")} onClose={onClose}>
    <div className="stack">
      <TextField label={app.t("Server ID", "服务器 ID")} value={draft.id} disabled={Boolean(initial)} onChange={(e) => patch({ id: e.target.value })} />
      <TextField label={app.t("Display name", "显示名称")} value={draft.name} onChange={(e) => patch({ name: e.target.value })} />
      <TextAreaField label={app.t("Description", "说明")} value={draft.description} onChange={(e) => patch({ description: e.target.value })} />
      <TextField label={app.t("Icon URL · optional", "图标 URL · 可选")} value={draft.icon ?? ""} placeholder="https://…" onChange={(e) => patch({ icon: e.target.value || null })} />
      <label className="ui-field"><span>{app.t("Transport", "传输方式")}</span><select className="ui-input" value={draft.transport.type}
        onChange={(e) => patch({ transport: e.target.value === "url" ? { type: "url", url: "" } : { type: "sandbox_stdio", command: "", args: [] } })}>
        <option value="url">HTTP</option><option value="sandbox_stdio">Sandbox stdio</option>
      </select></label>
      {draft.transport.type === "url" ? <TextField label="MCP URL" value={draft.transport.url} onChange={(e) => patch({ transport: { type: "url", url: e.target.value } })} />
        : <><TextField label={app.t("Sandbox command", "Sandbox 命令")} value={draft.transport.command} onChange={(e) => patch({ transport: { type: "sandbox_stdio", command: e.target.value, args: args.split("\n").filter(Boolean) } })} />
          <TextAreaField label={app.t("Arguments · one per line", "参数 · 每行一个")} value={args} onChange={(e) => { setArgs(e.target.value); if (draft.transport.type === "sandbox_stdio") patch({ transport: { ...draft.transport, args: e.target.value.split("\n").filter(Boolean) } }); }} />
          <p className="hint">{app.t("The command must exist inside the selected Session environment. Uploaded scripts use their /mnt/session/uploads path.", "命令必须存在于所选 Session 环境中；上传脚本使用 /mnt/session/uploads 下的路径。")}</p></>}
      <label><input type="checkbox" checked={draft.prompts_as_skills} onChange={(e) => patch({ prompts_as_skills: e.target.checked })} /> {app.t("Expose MCP prompts as Skills", "将 MCP Prompt 暴露为 Skill")}</label>
      {error && <div className="err">{error.message}</div>}
      <Button variant="primary" disabled={pending || !draft.id.trim() || !draft.name.trim()} onClick={() => onSave(draft)}>{app.t("Save server", "保存服务器")}</Button>
    </div>
  </Modal>;
}
