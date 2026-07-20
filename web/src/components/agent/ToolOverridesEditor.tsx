// Per-tool presentation overrides (ADR-0053): a row per override — the target tool id,
// an alias (rename for the model), a description override, and a defer toggle. `target`
// is an editable canonical id. Static tools are suggested, while runtime-discovered
// MCP tools are entered as `mcp__<server>__<tool>` without pretending they belong to
// the static tool catalog.

import { useId } from "react";
import { Button, Switch, TextField } from "../ui";
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
  const suggestionsId = `tool-targets-${useId().replaceAll(":", "")}`;
  const set = (i: number, patch: Partial<ToolOverride>) =>
    onChange(value.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  const add = (target = tools[0] ?? "") =>
    onChange([...value, { target, alias: "", description: "", defer: false }]);
  return (
    <>
      <datalist id={suggestionsId}>
        {tools.map((tool) => <option key={tool} value={tool} />)}
      </datalist>
      {value.map((r, i) => (
        <div key={i}>
          <div className="row" style={{ alignItems: "flex-end", gap: 8 }}>
            <TextField
              label={app.t("Canonical tool id", "规范工具 id")}
              mono
              list={suggestionsId}
              aria-label={`${app.t("Canonical tool id", "规范工具 id")} ${i + 1}`}
              style={{ minWidth: 160 }}
              value={r.target}
              onChange={(e) => set(i, { target: e.target.value })}
            />
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
          {r.target.startsWith("mcp__") && (
            <span className={isCanonicalMcpToolId(r.target) ? "mut" : "err"} style={{ display: "block", marginTop: 4, fontSize: 11 }}>
              {isCanonicalMcpToolId(r.target)
                ? app.t("Runtime-discovered MCP tool; resolved when the server connects.", "运行时发现的 MCP 工具；服务器连接后解析。")
                : app.t("Use mcp__server__tool so the runtime can match the discovered tool.", "请使用 mcp__server__tool，运行时才能匹配发现的工具。")}
            </span>
          )}
        </div>
      ))}
      <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
        <Button onClick={() => add()}>+ {app.t("override a selected tool", "覆盖已选工具")}</Button>
        <Button onClick={() => add("mcp__server__tool")}>+ {app.t("override an MCP tool", "覆盖 MCP 工具")}</Button>
      </div>
    </>
  );
}

/** MCP tool ids are dynamic and therefore cannot be enumerated at config compile.
 * The editor checks only the namespace shape; runtime discovery remains authority. */
export function isCanonicalMcpToolId(target: string): boolean {
  if (!target.startsWith("mcp__")) return false;
  const rest = target.slice("mcp__".length);
  const separator = rest.indexOf("__");
  return separator > 0 && separator < rest.length - 2;
}
