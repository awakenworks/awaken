// Managed-Agent integration fields that were previously reachable only through the
// API: direct MCP servers, Skill references, metadata, and multi-agent topology.
// Common shapes get compact controls; the full Agent JSON view remains the lossless
// editor for less common SDK union variants.

import { useEffect, useState } from "react";
import { Link } from "react-router";
import type { AgentConfig, CredentialSource } from "../../lib/api/types";
import { useApp } from "../../lib/app-state";
import { Button, Card, TextAreaField, TextField } from "../ui";

type JsonObject = Record<string, unknown>;

function objectOf(value: unknown): JsonObject {
  return value && typeof value === "object" && !Array.isArray(value) ? value as JsonObject : {};
}

function referenceId(value: unknown): string {
  if (typeof value === "string") return value;
  const object = objectOf(value);
  if (typeof object.skill_id === "string") return object.skill_id;
  return typeof object.id === "string" ? object.id : "";
}

function credentialValue(value: unknown): string {
  const credential = objectOf(value);
  return typeof credential.id === "string" && typeof credential.revision === "number"
    ? `${credential.id}@${credential.revision}`
    : "";
}

export default function AgentIntegrationsEditor({
  config,
  credentials,
  onChange,
  onValidityChange,
  section = "all",
}: {
  config: AgentConfig;
  credentials: CredentialSource[];
  onChange: (patch: Partial<AgentConfig>) => void;
  onValidityChange: (valid: boolean) => void;
  section?: "bindings" | "topology" | "all";
}) {
  const app = useApp();
  const servers = config.mcp_servers ?? [];
  const skills = config.skills ?? [];
  const metadata = config.metadata ?? {};
  const activeCredentials = credentials.filter((credential) => credential.status === "active");
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
      {(section === "bindings" || section === "all") && (
      <>
      <Card className="agent-config-card">
        <h2>{app.t("Direct MCP servers", "直接 MCP 服务器")}</h2>
        <p className="hint">
          {app.t(
            "MCP endpoints belong to this Agent draft and are frozen into its publication. Runtime tools are discovered when the server connects; policy and presentation remain under Tools.",
            "MCP endpoint 属于当前 Agent 草稿，并固化到发布版本中。运行时工具在服务器连接时发现；策略与呈现仍在 Tools 中配置。",
          )}
        </p>
        <div className="banner info">
          <span>ⓘ</span>
          <span>{app.t(
            "Credentials are exact secret-free references validated at publication. Session Vault ids remain a separate run-scoped mechanism.",
            "凭据是不含秘密的精确引用，并在发布时校验。Session Vault id 仍是独立的运行级机制。",
          )}{" "}
            <Link to={`/w/${app.workspaceId}/credentials`}>{app.t("Manage credential sources ↗", "管理凭据来源 ↗")}</Link>
          </span>
        </div>
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
              <label className="field">
                <span>{app.t("Credential source", "凭据来源")}</span>
                <select
                  className="input mono"
                  value={credentialValue(value.credential)}
                  onChange={(event) => {
                    const selected = activeCredentials.find((credential) =>
                      `${credential.id}@${credential.version}` === event.target.value);
                    setServer(index, {
                      credential: selected
                        ? { id: selected.id, revision: selected.version }
                        : undefined,
                    });
                  }}
                >
                  <option value="">{app.t("Unauthenticated", "无认证")}</option>
                  {activeCredentials.map((credential) => (
                    <option
                      key={`${credential.id}@${credential.version}`}
                      value={`${credential.id}@${credential.version}`}
                    >
                      {credential.id}@{credential.version} · {credential.kind}
                    </option>
                  ))}
                </select>
              </label>
              <label className="field" style={{ alignSelf: "center" }}>
                <span>{app.t("Prompts as skills", "将 Prompt 作为 Skill")}</span>
                <input
                  type="checkbox"
                  checked={value.prompts_as_skills === true}
                  onChange={(event) => setServer(index, { prompts_as_skills: event.target.checked })}
                />
              </label>
              <Button variant="ghost" onClick={() => onChange({ mcp_servers: servers.filter((_, i) => i !== index) })}>✕</Button>
            </div>
          );
        })}
        <Button onClick={() => onChange({ mcp_servers: [...servers, { type: "url", name: "", url: "", prompts_as_skills: false }] })}>
          + {app.t("MCP server", "MCP 服务器")}
        </Button>
      </Card>
      <Card className="agent-config-card">
        <h2>{app.t("Skill bindings", "Skill 绑定")}</h2>
        <p className="hint">
          {app.t(
            "Bind durable managed Skills so the operator can state only the goal. MCP Prompt Skills stay remote and instruction-only; enable Prompts as skills on the owning MCP server instead of creating fake local files.",
            "绑定持久化 Managed Skills，让操作者只需说明目标。MCP Prompt Skills 保持远程且仅含指令；请在对应 MCP 服务器上启用“Prompt 作为 Skill”，不要创建虚假的本地文件。",
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
      </>
      )}

      {(section === "topology" || section === "all") && (
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
      )}
    </div>
  );
}
