// Per-tool presentation overrides (ADR-0053): a row per override — the target tool id,
// an alias (rename for the model), a description override, and a defer toggle. `target`
// is picked from the agent's selected tools (add an MCP id under Tools to override it).

import { Button, SelectField, Switch, TextField } from "../ui";
import type { ToolOverride } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";

export default function ToolOverridesEditor({
  tools,
  value,
  onChange,
}: {
  tools: string[];
  value: ToolOverride[];
  onChange: (next: ToolOverride[]) => void;
}) {
  const app = useApp();
  const set = (i: number, patch: Partial<ToolOverride>) =>
    onChange(value.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const add = () =>
    onChange([...value, { target: tools[0] ?? "", alias: "", description: "", defer: false }]);
  return (
    <>
      {value.map((r, i) => (
        <div className="row" key={i} style={{ alignItems: "flex-end", gap: 8 }}>
          <SelectField
            label={app.t("Tool", "工具")}
            mono
            style={{ minWidth: 160 }}
            value={r.target}
            onChange={(e) => set(i, { target: e.target.value })}
          >
            {!tools.includes(r.target) && <option value={r.target}>{r.target || "—"}</option>}
            {tools.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </SelectField>
          <TextField label={app.t("Alias", "别名")} mono style={{ width: 130 }} placeholder="rename" value={r.alias ?? ""} onChange={(e) => set(i, { alias: e.target.value })} />
          <TextField label={app.t("Description", "描述")} style={{ flex: 1, minWidth: 160 }} placeholder="override description" value={r.description ?? ""} onChange={(e) => set(i, { description: e.target.value })} />
          <div className="field">
            <label>{app.t("Defer", "延迟")}</label>
            <div style={{ height: 30, display: "flex", alignItems: "center" }}>
              <Switch aria-label={app.t("Defer this tool", "延迟此工具")} checked={!!r.defer} onChange={(e) => set(i, { defer: e.target.checked })} />
            </div>
          </div>
          <Button variant="ghost" style={{ height: 30 }} onClick={() => onChange(value.filter((_, j) => j !== i))}>
            ✕
          </Button>
        </div>
      ))}
      <Button onClick={add}>+ {app.t("override a tool", "覆盖一个工具")}</Button>
    </>
  );
}
