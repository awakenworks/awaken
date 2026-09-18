import { Plus, X } from "@awaken/ui/icons";
import { Button, Switch } from "@awaken/ui";
// Per-tool presentation overrides (ADR-0053): a row per override — the target tool id,
// an alias (rename for the model), a description override, and an exposure toggle. `target`
// is an editable canonical id. Static tools are suggested, while runtime-discovered
// MCP tools are entered as `mcp__<server>__<tool>` without pretending they belong to
// the static tool catalog.

import { useId } from "react";
import { TextField } from "../ui";
import type { ToolOverride } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
// The `mcp__<server>__<tool>` grammar has one owner per side; the editor asks it
// rather than parsing the prefix a second time.
import { hasMcpToolNamespace, isCanonicalMcpToolId } from "../../lib/mcp-tool-id";

export default function ToolOverridesEditor({
  tools,
  value,
  onChange,
  backgroundTools = [],
  onBackgroundToolsChange,
  backgroundUnavailable = false,
  aliasUnavailable = false,
  descriptionUnavailable = false,
  discoveryUnavailable = false,
  lookupAliases = false,
}: {
  tools: string[];
  value: ToolOverride[];
  onChange: (next: ToolOverride[]) => void;
  backgroundTools?: string[];
  onBackgroundToolsChange?: (next: string[]) => void;
  backgroundUnavailable?: boolean;
  aliasUnavailable?: boolean;
  descriptionUnavailable?: boolean;
  discoveryUnavailable?: boolean;
  lookupAliases?: boolean;
}) {
  const app = useApp();
  const suggestionsId = `tool-targets-${useId().replaceAll(":", "")}`;
  const set = (i: number, patch: Partial<ToolOverride>) => {
    const previousTarget = value[i]?.target;
    onChange(value.map((r, j) => (j === i ? { ...r, ...patch } : r)));
    if (previousTarget && patch.target && backgroundTools.includes(previousTarget)) {
      onBackgroundToolsChange?.(retargetBackgroundTool(backgroundTools, previousTarget, patch.target));
    }
  };
  const add = (target = tools[0] ?? "") =>
    onChange([...value, { target }]);
  const setBackground = (target: string, enabled: boolean) => onBackgroundToolsChange?.(
    updateBackgroundEligibility(backgroundTools, target, enabled),
  );
  return (
    <>
      {lookupAliases && <p className="mut">{app.t("Lookup aliases redirect calls to canonical MCP tools. Schemas stay unchanged; configuring aliases disables builtin tools for the session.", "调用别名将调用路由到规范 MCP 工具；参数格式保持不变；配置非空调用别名后，将禁用该会话的内置工具。")}</p>}
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
            <TextField label={app.t("Alias", "别名")} disabled={aliasUnavailable} mono style={{ width: 130 }} placeholder="rename" value={r.alias ?? ""} onChange={(e) => set(i, { alias: e.target.value || undefined })} />
            <TextField label={app.t("Description", "描述")} disabled={descriptionUnavailable} style={{ flex: 1, minWidth: 160 }} placeholder="override description" value={r.description ?? ""} onChange={(e) => set(i, { description: e.target.value || undefined })} />
            <div className="ui-field">
              <label>{app.t("Show to model on demand", "仅在需要时向模型展示")}</label>
              <div style={{ height: 30, display: "flex", alignItems: "center" }}>
                <Switch
                  aria-label={app.t("Show this tool to the model on demand", "仅在需要时向模型展示此工具")}
                  disabled={discoveryUnavailable}
                  checked={r.exposure === "on_demand"}
                  onChange={(e) => set(i, { exposure: e.target.checked ? "on_demand" : undefined })}
                />
              </div>
            </div>
            {onBackgroundToolsChange && (
              <div className="ui-field">
                <label>{app.t("Allow background execution", "允许后台执行")}</label>
                <div style={{ height: 30, display: "flex", alignItems: "center" }}>
                  <Switch
                    aria-label={app.t("Allow this tool to run in the background", "允许此工具在后台执行")}
                    checked={backgroundTools.includes(r.target)}
                    disabled={backgroundUnavailable && !backgroundTools.includes(r.target)}
                    onChange={(e) => setBackground(r.target, e.target.checked)}
                  />
                </div>
              </div>
            )}
            <Button variant="ghost" aria-label={app.t(`Remove override ${r.target}`, `移除覆盖 ${r.target}`)} style={{ height: 30 }} onClick={() => {
              setBackground(r.target, false);
              onChange(value.filter((_, j) => j !== i));
            }}>
              <X />
            </Button>
          </div>
          {hasMcpToolNamespace(r.target) && (
            <span className={isCanonicalMcpToolId(r.target) ? "mut" : "err"} style={{ display: "block", marginTop: 4, fontSize: 11 }}>
              {isCanonicalMcpToolId(r.target)
                ? app.t("Runtime-discovered MCP tool; resolved when the server connects.", "运行时发现的 MCP 工具；服务器连接后解析。")
                : app.t("Use mcp__server__tool so the runtime can match the discovered tool.", "请使用 mcp__server__tool，运行时才能匹配发现的工具。")}
            </span>
          )}
        </div>
      ))}
      <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
        <Button onClick={() => add()}><Plus /> {app.t("override a selected tool", "覆盖已选工具")}</Button>
        <Button onClick={() => add("mcp__server__tool")}><Plus /> {app.t("override an MCP tool", "覆盖 MCP 工具")}</Button>
      </div>
      {onBackgroundToolsChange && (
        <span className="mut" style={{ display: "block", marginTop: 8, fontSize: 11 }}>
          {app.t(
            "Background execution does not grant a tool. Its ToolSet permission, state-machine guards, concurrency, recovery, and resource policies still apply.",
            "后台执行不会授予工具；其 ToolSet 权限、状态机守卫、并发、恢复和资源策略仍然生效。",
          )}
        </span>
      )}
    </>
  );
}

export function updateBackgroundEligibility(current: string[], target: string, enabled: boolean): string[] {
  if (!enabled) return current.filter((tool) => tool !== target);
  return target.trim() === "" ? current : Array.from(new Set([...current, target]));
}

export function retargetBackgroundTool(current: string[], previous: string, next: string): string[] {
  return Array.from(new Set(current.map((tool) => tool === previous ? next : tool))).filter(Boolean);
}
