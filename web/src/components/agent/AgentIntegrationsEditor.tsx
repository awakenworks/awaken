// Managed-Agent integration fields that were previously reachable only through the
// API: direct MCP servers, Skill references, metadata, and multi-agent topology.
// Common shapes get compact controls; the full Agent JSON view remains the lossless
// editor for less common SDK union variants.

import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo } from "react";
import { Link } from "react-router";
import type { AgentConfig, AgentMcpServer, AgentToolsetConfig, CredentialSource, ManagedToolsetCap, Page, Skill } from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import {
  isMcpToolset,
  mcpDefaultConfig,
  mcpIntegrationsValid,
  reconcileMcpToolsets,
  removeMcpIntegration,
  renameMcpIntegrationReferences,
} from "../../lib/agent-toolsets";
import { useApp } from "../../lib/app-state";
import { Button, Card, Switch, TextAreaField, TextField } from "../ui";
import AgentCollaborationEditor from "./AgentCollaborationEditor";

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

function mcpCredentialLabel(credential: CredentialSource, t: (en: string, zh: string) => string) {
  return `${t("MCP runtime credential", "MCP 运行时凭证")} · •••${credential.id.slice(-8)} · v${credential.version}`;
}

export default function AgentIntegrationsEditor({
  config,
  credentials,
  toolsetCapabilities,
  onChange,
  onValidityChange,
  section = "all",
}: {
  config: AgentConfig;
  credentials: CredentialSource[];
  toolsetCapabilities?: ManagedToolsetCap[];
  onChange: (patch: Partial<AgentConfig>) => void;
  onValidityChange: (valid: boolean) => void;
  section?: "bindings" | "topology" | "all";
}) {
  const app = useApp();
  const servers = config.mcp_servers ?? [];
  const skills = config.skills ?? [];
  const metadata = config.metadata ?? {};
  const defaultMcpPolicy = useMemo(() => mcpDefaultConfig(toolsetCapabilities), [toolsetCapabilities]);
  const skillCatalog = useQuery({
    queryKey: ["skills", app.workspaceId],
    queryFn: () => api.get<Page<Skill>>(ws("/v1/skills")),
  });
  const activeCredentials = credentials.filter((credential) =>
    credential.status === "active"
    && credential.kind !== "worker_local"
    && !credential.provider_id
    && !credential.env_key,
  );
  const syncServers = (nextServers: AgentMcpServer[], tools = config.tools, extra: Partial<AgentConfig> = {}) => {
    const { permission: _legacyPermission, ...pluginConfig } = config.plugin_config;
    onChange({
      ...extra,
      mcp_servers: nextServers,
      tools: reconcileMcpToolsets(tools, nextServers, defaultMcpPolicy),
      ...(nextServers.length > 0 ? { plugin_config: pluginConfig } : {}),
    });
  };
  const setServer = (index: number, patch: JsonObject) => {
    const previousName = servers[index]?.name ?? "";
    const nextServers = servers.map((server, i) => i === index ? { ...server, ...patch } as AgentMcpServer : server);
    const nextName = nextServers[index]?.name ?? previousName;
    const references = previousName === nextName ? {} : renameMcpIntegrationReferences(config, previousName, nextName);
    syncServers(nextServers, references.tools ?? config.tools, references);
  };
  const replaceServer = (index: number, replacement: AgentMcpServer) =>
    syncServers(servers.map((server, i) => i === index ? replacement : server));
  const setSkill = (index: number, id: string) =>
    onChange({ skills: skills.map((skill, i) => i === index ? { ...objectOf(skill), id } : skill) });
  const effectiveMcpValid = mcpIntegrationsValid({
    ...config,
    tools: reconcileMcpToolsets(config.tools, servers, defaultMcpPolicy),
  });
  useEffect(() => {
    const reconciled = reconcileMcpToolsets(config.tools, servers, defaultMcpPolicy);
    const hasLegacyPermission = servers.length > 0 && Object.hasOwn(config.plugin_config, "permission");
    if (JSON.stringify(reconciled) !== JSON.stringify(config.tools) || hasLegacyPermission) {
      const { permission: _legacyPermission, ...pluginConfig } = config.plugin_config;
      onChange({ tools: reconciled, ...(hasLegacyPermission ? { plugin_config: pluginConfig } : {}) });
    }
  }, [config.plugin_config, config.tools, defaultMcpPolicy, onChange, servers]);
  useEffect(() => onValidityChange(effectiveMcpValid), [effectiveMcpValid, onValidityChange]);

  return (
    <div className="agent-integration-stack">
      {(section === "bindings" || section === "all") && (
      <>
      <Card className="agent-config-card">
        <h2>{app.t("MCP integrations", "MCP 集成")}</h2>
        <p className="hint">
          {app.t(
            "Each server and its ToolSet policy are one integration. Connection, default permission, named overrides, and Prompt Skills stay together here.",
            "每个服务器及其 ToolSet 策略构成一个集成；连接、默认权限、指定工具覆盖和 Prompt Skills 都在这里配置。",
          )}
        </p>
        <div className="banner info">
          <span>ⓘ</span>
          <span>{app.t(
            "Only MCP runtime credentials are listed here; model keys and worker credentials are excluded. You can also choose a categorized Vault when starting a Session.",
            "这里只列出 MCP 运行时凭证，模型 Key 和 Worker 凭证不会混入。启动 Session 时也可以选择已分类的 Vault。",
          )}{" "}
            <Link to={`/w/${app.workspaceId}/vaults`}>{app.t("Manage Runtime Secrets ↗", "管理运行时凭证 ↗")}</Link>{" · "}
            <a
              href={`https://awakenworks.com${app.locale === "zh" ? "/zh" : ""}/docs/agents/protocols/mcp/`}
              target="_blank"
              rel="noreferrer"
            >
              {app.t("MCP connection and ToolSet guide ↗", "MCP 连接与 ToolSet 指南 ↗")}
            </a>
          </span>
        </div>
        {servers.map((server, index) => {
          const sandboxStdio = server.type === "sandbox_stdio";
          const toolset = config.tools.filter(isMcpToolset).find((tool) => tool.mcp_server_name === server.name);
          const defaults = toolset?.default_config ?? defaultMcpPolicy;
          const configs = toolset?.configs ?? [];
          const updateToolset = (patch: { default_config?: typeof defaults; configs?: AgentToolsetConfig[] }) => onChange({
            tools: reconcileMcpToolsets(config.tools, servers, defaultMcpPolicy).map((tool) =>
              isMcpToolset(tool) && tool.mcp_server_name === server.name ? { ...tool, ...patch } : tool),
          });
          return (
            <div className="agent-config-card mcp-integration-card" key={index}>
            <div className="row" style={{ justifyContent: "space-between" }}>
              <strong>{app.t(`MCP integration ${index + 1}`, `MCP 集成 ${index + 1}`)}</strong>
              <span className="badge ok">{app.t("ToolSet policy attached", "已关联 ToolSet 策略")}</span>
            </div>
            <div className="agent-integration-row">
              <label className="field">
                <span>{app.t("Transport", "传输方式")}</span>
                <select
                  className="input mono"
                  value={sandboxStdio ? "sandbox_stdio" : "url"}
                  onChange={(event) => replaceServer(index, event.target.value === "sandbox_stdio"
                    ? {
                        type: "sandbox_stdio",
                        name: server.name,
                        command: "",
                        args: [],
                        prompts_as_skills: server.prompts_as_skills === true,
                      }
                    : {
                        type: "url",
                        name: server.name,
                        url: "",
                        prompts_as_skills: server.prompts_as_skills === true,
                      })}
                >
                  <option value="url">HTTP</option>
                  <option value="sandbox_stdio">{app.t("Sandbox stdio", "Sandbox stdio")}</option>
                </select>
              </label>
              <TextField
                label={app.t("Server name", "服务器名称")}
                mono
                value={server.name}
                onChange={(event) => setServer(index, { name: event.target.value })}
              />
              {sandboxStdio ? (
                <>
                  <TextField
                    label={app.t("Sandbox command", "Sandbox 命令")}
                    mono
                    placeholder="playwright-mcp"
                    value={server.command}
                    onChange={(event) => setServer(index, { command: event.target.value })}
                  />
                  <TextAreaField
                    label={app.t("Arguments (one per line)", "参数（每行一个）")}
                    mono
                    rows={3}
                    value={(server.args ?? []).join("\n")}
                    onChange={(event) => setServer(index, {
                      args: event.target.value.split("\n").filter((arg) => arg.length > 0),
                    })}
                  />
                </>
              ) : (
                <TextField
                  label="URL"
                  mono
                  placeholder="https://mcp.example.com"
                  value={server.url}
                  onChange={(event) => setServer(index, { type: "url", url: event.target.value })}
                />
              )}
              {!sandboxStdio && (
              <label className="field">
                <span>{app.t("Credential source", "凭据来源")}</span>
                <select
                  className="input mono"
                  value={credentialValue(server.credential)}
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
                      {mcpCredentialLabel(credential, app.t)}
                    </option>
                  ))}
                </select>
              </label>
              )}
              <label className="field" style={{ alignSelf: "center" }}>
                <span>{app.t("Prompts as skills", "将 Prompt 作为 Skill")}</span>
                <input
                  type="checkbox"
                  checked={server.prompts_as_skills === true}
                  onChange={(event) => setServer(index, { prompts_as_skills: event.target.checked })}
                />
              </label>
              <Button aria-label={app.t(`Remove ${server.name || "MCP"} integration`, `移除 ${server.name || "MCP"} 集成`)} variant="ghost" onClick={() => onChange(removeMcpIntegration(config, server.name))}>✕</Button>
            </div>
            <div className="mcp-policy-row">
              <label className="field">
                <span>{app.t("Default permission", "默认权限")}</span>
                <select aria-label={app.t("Default permission", "默认权限")} className="input" value={defaults?.permission_policy?.type ?? "always_ask"} onChange={(event) => updateToolset({
                  default_config: { ...defaults, permission_policy: { type: event.target.value as "always_allow" | "always_ask" } },
                })}>
                  <option value="always_ask">{app.t("Ask before use", "使用前询问")}</option>
                  <option value="always_allow">{app.t("Allow without asking", "无需询问即可使用")}</option>
                </select>
              </label>
              <label className="field compact-switch">
                <span>{app.t("Available by default", "默认可用")}</span>
                <Switch checked={defaults?.enabled !== false} onChange={(event) => updateToolset({ default_config: { ...defaults, enabled: event.target.checked } })} />
              </label>
              <span className="mut">{app.t("New tools discovered from this server inherit this policy.", "从此服务器新发现的工具会继承该策略。")}</span>
            </div>
            {configs.map((entry, configIndex) => (
              <div className="agent-integration-row mcp-tool-override" key={`${entry.name}-${configIndex}`}>
                <TextField label={app.t("Named tool override", "指定工具覆盖")} mono value={entry.name} onChange={(event) => updateToolset({ configs: configs.map((item, current) => current === configIndex ? { ...item, name: event.target.value } : item) })} />
                <label className="field"><span>{app.t("Tool permission", "工具权限")}</span><select aria-label={app.t("Tool permission", "工具权限")} className="input" value={entry.permission_policy?.type ?? defaults?.permission_policy?.type ?? "always_ask"} onChange={(event) => updateToolset({ configs: configs.map((item, current) => current === configIndex ? { ...item, permission_policy: { type: event.target.value as "always_allow" | "always_ask" } } : item) })}><option value="always_ask">{app.t("Ask", "询问")}</option><option value="always_allow">{app.t("Allow", "允许")}</option></select></label>
                <label className="field compact-switch"><span>{app.t("Available", "可用")}</span><Switch checked={entry.enabled !== false} onChange={(event) => updateToolset({ configs: configs.map((item, current) => current === configIndex ? { ...item, enabled: event.target.checked } : item) })} /></label>
                <Button aria-label={app.t("Remove named override", "移除指定工具覆盖")} variant="ghost" onClick={() => updateToolset({ configs: configs.filter((_, current) => current !== configIndex) })}>✕</Button>
              </div>
            ))}
            <Button variant="ghost" onClick={() => updateToolset({ configs: [...configs, { name: "", enabled: true, permission_policy: { type: defaults?.permission_policy?.type ?? "always_ask" } }] })}>+ {app.t("Named tool override", "指定工具覆盖")}</Button>
            </div>
          );
        })}
        <Button onClick={() => syncServers([...servers, { type: "url", name: "", url: "", prompts_as_skills: false }])}>
          + {app.t("MCP integration", "MCP 集成")}
        </Button>
        {!effectiveMcpValid && <div className="err" role="alert">{app.t(
          "Complete every server name and connection target, and keep server names unique. Awaken maintains one ToolSet policy for each valid server.",
          "请补全每个服务器名称和连接目标，并确保服务器名称唯一。Awaken 会为每个有效服务器维护一个 ToolSet 策略。",
        )}</div>}
      </Card>
      <Card className="agent-config-card">
        <h2>{app.t("Skill bindings", "Skill 绑定")}</h2>
        <p className="hint">
          {app.t(
            "Bind durable managed Skills so the operator can state only the goal. MCP Prompt Skills stay remote and instruction-only; enable Prompts as skills on the owning MCP server instead of creating fake local files.",
            "绑定持久化 Managed Skills，让操作者只需说明目标。MCP Prompt Skills 保持远程且仅含指令；请在对应 MCP 服务器上启用“Prompt 作为 Skill”，不要创建虚假的本地文件。",
          )}
        </p>
        {skills.map((skill, index) => {
          const selectedId = referenceId(skill);
          const catalog = skillCatalog.data?.data ?? [];
          return (
          <div className="agent-integration-row" key={index}>
            <label className="field">
              <span>{app.t("Skill", "Skill")}</span>
              <select
                aria-label={app.t("Skill", "Skill")}
                className="input"
                value={selectedId}
                onChange={(event) => setSkill(index, event.target.value)}
              >
                <option value="">{app.t("Choose a published Skill…", "选择已发布 Skill…")}</option>
                {selectedId && !catalog.some((candidate) => candidate.id === selectedId) && <option value={selectedId}>{selectedId}</option>}
                {catalog.map((candidate) => (
                  <option key={candidate.id} value={candidate.id}>
                    {candidate.display_title ?? candidate.name ?? candidate.display_name ?? candidate.id} · v{candidate.latest_version ?? "—"}
                  </option>
                ))}
              </select>
            </label>
            <Button variant="ghost" onClick={() => onChange({ skills: skills.filter((_, i) => i !== index) })}>✕</Button>
          </div>
          );
        })}
        {skillCatalog.error instanceof Error && <div className="err">{app.t("Skills could not load. Existing bindings are unchanged.", "无法加载 Skill；已有绑定未发生变化。")}</div>}
        <Button onClick={() => onChange({ skills: [...skills, { id: "" }] })}>+ Skill</Button>
      </Card>
      </>
      )}

      {(section === "topology" || section === "all") && (
      <>
      <AgentCollaborationEditor config={config} onChange={onChange} onValidityChange={onValidityChange} />
      <Card className="agent-config-card">
        <h2>{app.t("Metadata", "元数据")}</h2>
        <p className="hint">
          {app.t("Attach searchable labels and ownership information. Runtime behavior belongs in the typed controls above, not in metadata.", "添加便于搜索的标签与归属信息。运行时行为应使用上方结构化控件，不应放入元数据。")}
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
      </Card>
      </>
      )}
    </div>
  );
}
