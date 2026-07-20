// Managed-Agent integration fields that were previously reachable only through the
// API: direct MCP servers, Skill references, metadata, and multi-agent topology.
// Common shapes get compact controls; the full Agent JSON view remains the lossless
// editor for less common SDK union variants.

import { useEffect, useState } from "react";
import type { AgentConfig } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { Button, Card, TextAreaField, TextField } from "../ui";

type JsonObject = Record<string, unknown>;

function objectOf(value: unknown): JsonObject {
  return value && typeof value === "object" && !Array.isArray(value) ? value as JsonObject : {};
}

function referenceId(value: unknown): string {
  if (typeof value === "string") return value;
  const object = objectOf(value);
  return typeof object.id === "string" ? object.id : "";
}

export default function AgentIntegrationsEditor({
  config,
  onChange,
  onValidityChange,
}: {
  config: AgentConfig;
  onChange: (patch: Partial<AgentConfig>) => void;
  onValidityChange: (valid: boolean) => void;
}) {
  const app = useApp();
  const servers = config.mcp_servers ?? [];
  const skills = config.skills ?? [];
  const metadata = config.metadata ?? {};
  const [multiagentText, setMultiagentText] = useState(() =>
    config.multiagent == null ? "" : JSON.stringify(config.multiagent, null, 2));
  const [multiagentError, setMultiagentError] = useState("");

  useEffect(() => {
    setMultiagentText(config.multiagent == null ? "" : JSON.stringify(config.multiagent, null, 2));
  }, [config.multiagent]);

  const setServer = (index: number, patch: JsonObject) =>
    onChange({ mcp_servers: servers.map((server, i) => i === index ? { ...objectOf(server), ...patch } : server) });
  const setSkill = (index: number, id: string) =>
    onChange({ skills: skills.map((skill, i) => i === index ? { ...objectOf(skill), id } : skill) });

  return (
    <div className="agent-integration-stack">
      <Card className="agent-config-card">
        <h2>{app.t("Direct MCP servers", "直接 MCP 服务器")}</h2>
        <p className="hint">
          {app.t(
            "Managed Agents-compatible name + URL bindings. Runtime tools appear as mcp__server__tool and can be renamed or deferred in Tools.",
            "兼容 Managed Agents 的名称 + URL 绑定。运行时工具以 mcp__server__tool 出现，可在 Tools 中改名或延迟加载。",
          )}
        </p>
        {servers.map((server, index) => {
          const value = objectOf(server);
          return (
            <div className="agent-integration-row" key={index}>
              <TextField
                label={app.t("Server name", "服务器名称")}
                mono
                value={typeof value.name === "string" ? value.name : ""}
                onChange={(event) => setServer(index, { type: "url", name: event.target.value })}
              />
              <TextField
                label="URL"
                mono
                placeholder="https://mcp.example.com"
                value={typeof value.url === "string" ? value.url : ""}
                onChange={(event) => setServer(index, { type: "url", url: event.target.value })}
              />
              <Button variant="ghost" onClick={() => onChange({ mcp_servers: servers.filter((_, i) => i !== index) })}>✕</Button>
            </div>
          );
        })}
        <Button onClick={() => onChange({ mcp_servers: [...servers, { type: "url", name: "", url: "" }] })}>
          + {app.t("MCP server", "MCP 服务器")}
        </Button>
      </Card>

      <Card className="agent-config-card">
        <h2>{app.t("Skill optimization", "Skill 优化")}</h2>
        <p className="hint">
          {app.t(
            "Bind focused instructions so the operator can state only the goal; the Agent discovers and activates detailed procedure at runtime.",
            "绑定聚焦的指令，让用户只需说明目标；Agent 在运行时发现并激活详细流程。",
          )}
        </p>
        {skills.map((skill, index) => (
          <div className="agent-integration-row" key={index}>
            <TextField
              label="Skill id"
              mono
              value={referenceId(skill)}
              onChange={(event) => setSkill(index, event.target.value)}
            />
            <Button variant="ghost" onClick={() => onChange({ skills: skills.filter((_, i) => i !== index) })}>✕</Button>
          </div>
        ))}
        <Button onClick={() => onChange({ skills: [...skills, { id: "" }] })}>+ Skill</Button>
      </Card>

      <Card className="agent-config-card">
        <h2>{app.t("Metadata & multi-agent topology", "元数据与多 Agent 拓扑")}</h2>
        <p className="hint">
          {app.t("Metadata is editable as key/value pairs. Multi-agent stays JSON because the Managed Agents field is an open union.", "元数据按键值编辑。multiagent 是 Managed Agents 的开放联合类型，因此保留 JSON 编辑。")}
        </p>
        {Object.entries(metadata).map(([key, value], index) => (
          <div className="agent-integration-row" key={`${key}-${index}`}>
            <TextField
              label={app.t("Metadata key", "元数据键")}
              mono
              value={key}
              onChange={(event) => {
                const next = { ...metadata };
                delete next[key];
                next[event.target.value] = value;
                onChange({ metadata: next });
              }}
            />
            <TextField label={app.t("Value", "值")} value={value} onChange={(event) => onChange({ metadata: { ...metadata, [key]: event.target.value } })} />
            <Button variant="ghost" onClick={() => {
              const next = { ...metadata };
              delete next[key];
              onChange({ metadata: next });
            }}>✕</Button>
          </div>
        ))}
        <Button onClick={() => onChange({ metadata: { ...metadata, [`key_${Object.keys(metadata).length + 1}`]: "" } })}>
          + {app.t("metadata", "元数据")}
        </Button>
        <TextAreaField
          label="multiagent JSON"
          mono
          rows={6}
          placeholder='{"agents":[...]}'
          value={multiagentText}
          onChange={(event) => {
            const raw = event.target.value;
            setMultiagentText(raw);
            try {
              onChange({ multiagent: raw.trim() ? JSON.parse(raw) : undefined });
              onValidityChange(true);
              setMultiagentError("");
            } catch {
              onValidityChange(false);
              setMultiagentError(app.t("Invalid JSON — not applied.", "JSON 非法——未应用。"));
            }
          }}
        />
        {multiagentError && <span className="err" role="alert">{multiagentError}</span>}
      </Card>
    </div>
  );
}
