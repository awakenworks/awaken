// Managed-Agent integration fields that were previously reachable only through the
// API: direct MCP servers, Skill references, metadata, and multi-agent topology.
// Common shapes get compact controls; the full Agent JSON view remains the lossless
// editor for less common SDK union variants.

import { useQuery } from "@tanstack/react-query";
import { Link } from "react-router";
import type { AgentConfig, CredentialSource, Page, Skill } from "../../lib/api/types";
import { api, ws } from "../../lib/api/client";
import { useApp } from "../../lib/app-state";
import { Button, Card, TextAreaField, TextField } from "../ui";
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
  const setServer = (index: number, patch: JsonObject) =>
    onChange({ mcp_servers: servers.map((server, i) => i === index ? { ...objectOf(server), ...patch } : server) });
  const replaceServer = (index: number, replacement: JsonObject) =>
    onChange({ mcp_servers: servers.map((server, i) => i === index ? replacement : server) });
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
            "Add the MCP servers this Agent may connect to. Their tools become available after a successful connection; configure tool permissions and labels under Tools.",
            "添加此 Agent 可以连接的 MCP 服务器。连接成功后即可使用其工具；工具权限和名称在“工具”中配置。",
          )}
        </p>
        <div className="banner info">
          <span>ⓘ</span>
          <span>{app.t(
            "Only MCP runtime credentials are listed here; model keys and worker credentials are excluded. You can also choose a categorized Vault when starting a Session.",
            "这里只列出 MCP 运行时凭证，模型 Key 和 Worker 凭证不会混入。启动 Session 时也可以选择已分类的 Vault。",
          )}{" "}
            <Link to={`/w/${app.workspaceId}/vaults`}>{app.t("Manage Runtime Secrets ↗", "管理运行时凭证 ↗")}</Link>
          </span>
        </div>
        {servers.map((server, index) => {
          const value = objectOf(server);
          const sandboxStdio = value.type === "sandbox_stdio";
          return (
            <div className="agent-integration-row" key={index}>
              <label className="field">
                <span>{app.t("Transport", "传输方式")}</span>
                <select
                  className="input mono"
                  value={sandboxStdio ? "sandbox_stdio" : "url"}
                  onChange={(event) => replaceServer(index, event.target.value === "sandbox_stdio"
                    ? {
                        type: "sandbox_stdio",
                        name: typeof value.name === "string" ? value.name : "",
                        command: "",
                        args: [],
                        prompts_as_skills: value.prompts_as_skills === true,
                      }
                    : {
                        type: "url",
                        name: typeof value.name === "string" ? value.name : "",
                        url: "",
                        prompts_as_skills: value.prompts_as_skills === true,
                      })}
                >
                  <option value="url">HTTP</option>
                  <option value="sandbox_stdio">{app.t("Sandbox stdio", "Sandbox stdio")}</option>
                </select>
              </label>
              <TextField
                label={app.t("Server name", "服务器名称")}
                mono
                value={typeof value.name === "string" ? value.name : ""}
                onChange={(event) => setServer(index, { name: event.target.value })}
              />
              {sandboxStdio ? (
                <>
                  <TextField
                    label={app.t("Sandbox command", "Sandbox 命令")}
                    mono
                    placeholder="playwright-mcp"
                    value={typeof value.command === "string" ? value.command : ""}
                    onChange={(event) => setServer(index, { command: event.target.value })}
                  />
                  <TextAreaField
                    label={app.t("Arguments (one per line)", "参数（每行一个）")}
                    mono
                    rows={3}
                    value={Array.isArray(value.args) ? value.args.filter((arg): arg is string => typeof arg === "string").join("\n") : ""}
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
                  value={typeof value.url === "string" ? value.url : ""}
                  onChange={(event) => setServer(index, { type: "url", url: event.target.value })}
                />
              )}
              {!sandboxStdio && (
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
        {skills.map((skill, index) => {
          const selectedId = referenceId(skill);
          const catalog = skillCatalog.data?.data ?? [];
          return (
          <div className="agent-integration-row" key={index}>
            <label className="field">
              <span>{app.t("Skill", "Skill")}</span>
              <select className="input" value={selectedId} onChange={(event) => setSkill(index, event.target.value)}>
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
