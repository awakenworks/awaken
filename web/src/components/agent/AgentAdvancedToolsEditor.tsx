// Complete Agent tool authoring beyond the ordinary catalog checklist: dynamic
// catalog patterns, client-executed tools, typed toolset policy, and bounded
// recovery behavior. All values remain part of the same reviewed draft.

import { useEffect, useMemo, useState } from "react";
import type { AgentConfig, AgentManagedToolset, AgentToolsetConfig, CustomClientTool } from "../../lib/api/types";
import { isAgentToolset, isMcpToolset } from "../../lib/agent-toolsets";
import { Button, Card, Switch, TextAreaField, TextField } from "../ui";
import { useApp } from "../../lib/app-state";

type RecoveryPolicy = { mode: "never_replay" | "replay_safe" | "idempotent" | "durable_request"; max_attempts: number };

function isCustom(tool: AgentConfig["tools"][number]): tool is CustomClientTool {
  return typeof tool === "object" && tool.type === "custom";
}

function ClientToolSchemaEditor({
  value,
  onChange,
  onValidityChange,
}: {
  value: Record<string, unknown>;
  onChange: (value: Record<string, unknown>) => void;
  onValidityChange: (valid: boolean) => void;
}) {
  const app = useApp();
  const serialized = useMemo(() => JSON.stringify(value, null, 2), [value]);
  const [draft, setDraft] = useState(serialized);
  const [error, setError] = useState(false);
  useEffect(() => {
    if (!error) setDraft(serialized);
  }, [error, serialized]);
  return (
    <>
      <TextAreaField
        label={app.t("Input JSON Schema", "输入 JSON Schema")}
        mono rows={4}
        value={draft}
        onChange={(event) => {
          const next = event.target.value;
          setDraft(next);
          try {
            const schema = JSON.parse(next) as Record<string, unknown>;
            if (!schema || Array.isArray(schema) || schema.type !== "object") {
              throw new Error("object schema required");
            }
            setError(false);
            onValidityChange(true);
            onChange(schema);
          } catch {
            setError(true);
            onValidityChange(false);
          }
        }}
      />
      {error && <div className="err" role="alert">{app.t("Use a valid JSON object schema before saving.", "保存前请输入有效的 JSON 对象 Schema。")}</div>}
    </>
  );
}

export default function AgentAdvancedToolsEditor({
  config,
  onPatch,
  onValidityChange,
}: {
  config: AgentConfig;
  onPatch: (patch: Partial<AgentConfig>) => void;
  onValidityChange: (valid: boolean) => void;
}) {
  const app = useApp();
  const staticTools = config.tools.filter((tool): tool is string => typeof tool === "string");
  const customTools = config.tools.filter(isCustom);
  const toolsets = config.tools.filter(isAgentToolset);
  const mcpToolsets = config.tools.filter(isMcpToolset);
  const otherTools = config.tools.filter((tool) => typeof tool !== "string" && !isCustom(tool) && !isAgentToolset(tool) && !isMcpToolset(tool));
  const [invalidSchemas, setInvalidSchemas] = useState<Set<number>>(new Set());
  useEffect(() => onValidityChange(invalidSchemas.size === 0), [invalidSchemas, onValidityChange]);

  const replaceAdvanced = (custom: CustomClientTool[], sets: AgentManagedToolset[]) => {
    const { permission: _legacyPermission, ...pluginConfig } = config.plugin_config;
    onPatch({
      tools: [...otherTools, ...mcpToolsets, ...sets, ...custom, ...staticTools],
      ...((mcpToolsets.length > 0 || sets.length > 0) ? { plugin_config: pluginConfig } : {}),
    });
  };
  const setCustom = (index: number, patch: Partial<CustomClientTool>) => replaceAdvanced(
    customTools.map((tool, current) => current === index ? { ...tool, ...patch } : tool),
    toolsets,
  );
  const setToolset = (index: number, patch: Partial<AgentManagedToolset>) => replaceAdvanced(
    customTools,
    toolsets.map((toolset, current) => current === index ? { ...toolset, ...patch } : toolset),
  );
  const recovery = (config.recovery_policies ?? {}) as Record<string, RecoveryPolicy>;
  const replaceRecoveryKey = (previous: string, next: string) => {
    const value = recovery[previous];
    const updated = { ...recovery };
    delete updated[previous];
    updated[next] = value;
    onPatch({ recovery_policies: updated });
  };

  return (
    <Card style={{ marginTop: 14 }}>
      <h2 className="section-title">{app.t("Advanced tool sources and recovery", "高级工具来源与恢复策略")}</h2>
      <p className="hint">{app.t(
        "Use the catalog above for ordinary tools. These controls cover dynamic catalog families, tools executed by an application client, typed toolset policy, and crash recovery.",
        "普通工具请使用上方目录；这里配置动态工具族、由应用客户端执行的工具、结构化工具集策略和崩溃恢复行为。",
      )}</p>

      <TextAreaField
        label={app.t("Tool id patterns (one per line)", "工具 ID 模式（每行一个）")}
        hint={app.t("Patterns add matching catalog tools at publication; an unmatched pattern grants nothing.", "发布时加入匹配的目录工具；没有匹配项的模式不会授予任何能力。")}
        mono rows={3}
        value={(config.tool_patterns ?? []).join("\n")}
        onChange={(event) => onPatch({ tool_patterns: event.target.value.split("\n").map((value) => value.trim()).filter(Boolean) })}
      />

      <div className="field">
        <label>{app.t("Custom tools · client executed", "Custom Tool · 客户端执行")}</label>
        <span className="mut">{app.t(
          "The model may request these tools, but the Session waits for the trusted application client to return the result.",
          "模型可以请求这些工具，但 Session 会等待可信应用客户端返回执行结果。",
        )}</span>
        {customTools.map((tool, index) => (
          <div className="agent-config-card" key={`${tool.name}-${index}`} style={{ padding: 12, marginTop: 8 }}>
            <div className="row" style={{ alignItems: "flex-end" }}>
              <TextField label={app.t("Tool name", "工具名称")} mono value={tool.name} onChange={(event) => setCustom(index, { name: event.target.value })} />
              <TextField label={app.t("Description shown to the model", "向模型展示的描述")} style={{ flex: 1 }} value={tool.description} onChange={(event) => setCustom(index, { description: event.target.value })} />
              <Button variant="ghost" onClick={() => {
                setInvalidSchemas((current) => new Set(
                  [...current]
                    .filter((invalidIndex) => invalidIndex !== index)
                    .map((invalidIndex) => invalidIndex > index ? invalidIndex - 1 : invalidIndex),
                ));
                replaceAdvanced(customTools.filter((_, current) => current !== index), toolsets);
              }}>✕</Button>
            </div>
            <ClientToolSchemaEditor
              value={tool.input_schema}
              onChange={(schema) => setCustom(index, { input_schema: schema })}
              onValidityChange={(valid) => setInvalidSchemas((current) => {
                const next = new Set(current);
                if (valid) next.delete(index);
                else next.add(index);
                return next;
              })}
            />
          </div>
        ))}
        <Button onClick={() => replaceAdvanced([
          ...customTools,
          { type: "custom", name: "", description: "", input_schema: { type: "object", properties: {} } },
        ], toolsets)}>+ {app.t("Custom tool", "Custom Tool")}</Button>
      </div>

      <div className="field">
        <label>{app.t("Built-in Agent ToolSet", "内置 Agent ToolSet")}</label>
        <span className="mut">{app.t(
          "Set the default for Awaken's built-in Agent tools, then override named tools only where needed. MCP policies live with their server under MCP integrations.",
          "设置 Awaken 内置 Agent 工具的默认行为，仅在必要时覆盖指定工具；MCP 策略与服务器一起位于 MCP 集成中。",
        )}</span>
        {toolsets.map((toolset, index) => {
          const defaults = toolset.default_config ?? { enabled: true, permission_policy: { type: "always_allow" as const } };
          const configs = toolset.configs ?? [];
          const setConfig = (configIndex: number, patch: Partial<AgentToolsetConfig>) => setToolset(index, {
            configs: configs.map((entry, current) => current === configIndex ? { ...entry, ...patch } : entry),
          });
          return (
            <div className="agent-config-card" key={index} style={{ padding: 12, marginTop: 8 }}>
              <div className="row" style={{ alignItems: "flex-end" }}>
                <label className="field">
                  <span>{app.t("Default permission", "默认权限")}</span>
                  <select className="input" value={defaults.permission_policy?.type ?? "always_allow"} onChange={(event) => setToolset(index, { default_config: { ...defaults, permission_policy: { type: event.target.value as "always_allow" | "always_ask" } } })}>
                    <option value="always_allow">always_allow</option>
                    <option value="always_ask">always_ask</option>
                  </select>
                </label>
                <label className="field">
                  <span>{app.t("Enabled by default", "默认启用")}</span>
                  <Switch checked={defaults.enabled !== false} onChange={(event) => setToolset(index, { default_config: { ...defaults, enabled: event.target.checked } })} />
                </label>
                <Button variant="ghost" onClick={() => replaceAdvanced(customTools, toolsets.filter((_, current) => current !== index))}>✕</Button>
              </div>
              {configs.map((entry, configIndex) => (
                <div className="row" style={{ alignItems: "flex-end" }} key={configIndex}>
                  <TextField label={app.t("Tool override", "覆盖工具")} mono value={entry.name} onChange={(event) => setConfig(configIndex, { name: event.target.value })} />
                  <label className="field"><span>{app.t("Permission", "权限")}</span><select className="input" value={entry.permission_policy?.type ?? "always_allow"} onChange={(event) => setConfig(configIndex, { permission_policy: { type: event.target.value as "always_allow" | "always_ask" } })}><option value="always_allow">always_allow</option><option value="always_ask">always_ask</option></select></label>
                  <label className="field"><span>{app.t("Enabled", "启用")}</span><Switch checked={entry.enabled !== false} onChange={(event) => setConfig(configIndex, { enabled: event.target.checked })} /></label>
                  <Button variant="ghost" onClick={() => setToolset(index, { configs: configs.filter((_, current) => current !== configIndex) })}>✕</Button>
                </div>
              ))}
              <Button onClick={() => setToolset(index, { configs: [...configs, { name: "", enabled: true, permission_policy: { type: "always_allow" } }] })}>+ {app.t("named override", "指定工具覆盖")}</Button>
            </div>
          );
        })}
        {toolsets.length === 0 && <Button onClick={() => replaceAdvanced(customTools, [...toolsets, {
          type: "agent_toolset_20260401", configs: [],
          default_config: { enabled: true, permission_policy: { type: "always_allow" } },
        }])}>+ {app.t("Configure built-in ToolSet", "配置内置 ToolSet")}</Button>}
      </div>

      <div className="field">
        <label>{app.t("Crash recovery by tool", "按工具配置崩溃恢复")}</label>
        <span className="mut">{app.t(
          "Recovery never grants a tool. Safer replay modes are accepted only when the executable tool advertises the matching guarantee.",
          "恢复策略不会授予工具能力；仅当工具声明了对应保证时，运行时才接受更积极的重试模式。",
        )}</span>
        {Object.entries(recovery).map(([toolId, policy], index) => (
          <div className="row" style={{ alignItems: "flex-end" }} key={`${toolId}-${index}`}>
            <TextField label={app.t("Canonical tool id", "规范工具 ID")} mono value={toolId} onChange={(event) => replaceRecoveryKey(toolId, event.target.value)} />
            <label className="field"><span>{app.t("Recovery mode", "恢复模式")}</span><select className="input" value={policy.mode} onChange={(event) => onPatch({ recovery_policies: { ...recovery, [toolId]: { ...policy, mode: event.target.value as RecoveryPolicy["mode"] } } })}><option value="never_replay">never_replay</option><option value="replay_safe">replay_safe</option><option value="idempotent">idempotent</option><option value="durable_request">durable_request</option></select></label>
            <TextField label={app.t("Maximum attempts", "最大尝试次数")} type="number" min={1} value={policy.max_attempts} onChange={(event) => onPatch({ recovery_policies: { ...recovery, [toolId]: { ...policy, max_attempts: Math.max(1, Number(event.target.value) || 1) } } })} />
            <Button variant="ghost" onClick={() => { const next = { ...recovery }; delete next[toolId]; onPatch({ recovery_policies: next }); }}>✕</Button>
          </div>
        ))}
        <Button onClick={() => onPatch({ recovery_policies: { ...recovery, [`tool_${Object.keys(recovery).length + 1}`]: { mode: "never_replay", max_attempts: 3 } } })}>+ {app.t("recovery policy", "恢复策略")}</Button>
      </div>
    </Card>
  );
}
