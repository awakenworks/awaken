// Workspace · Environments: the reusable execution templates sessions run in
// (Managed Agents `/v1/environments`). A session references one by
// `environment_id`. cloud = platform-managed; self_hosted = your own worker.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api, ws } from "../lib/api/client";
import type {
  Environment,
  EnvironmentConfig,
  EnvironmentPackages,
  Page,
  SandboxExecutionPolicy,
  SandboxPolicyBinding,
  SandboxProvisioning,
  WorkQueueStats,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Button, Card, Modal, Pill, Segmented, TechnicalId, TextAreaField, TextField, useConfirm, useToast } from "../components/ui";

const PACKAGE_MANAGERS = ["apt", "cargo", "gem", "go", "npm", "pip"] as const;
type PackageManager = typeof PACKAGE_MANAGERS[number];
type PackageDraft = Record<PackageManager, string>;
type EnvironmentUpdateConfig = EnvironmentConfig | (Omit<EnvironmentConfig, "packages"> & { packages: null });

const emptyPackages = (): PackageDraft => ({ apt: "", cargo: "", gem: "", go: "", npm: "", pip: "" });

export function parsePackageList(value: string): string[] {
  return value.split(/[\n,]/).map((item) => item.trim()).filter(Boolean);
}

function packageDraft(packages?: EnvironmentPackages): PackageDraft {
  return Object.fromEntries(PACKAGE_MANAGERS.map((manager) => [manager, packages?.[manager]?.join("\n") ?? ""])) as PackageDraft;
}

function hasPackageRequirements(packages: PackageDraft): boolean {
  return PACKAGE_MANAGERS.some((manager) => parsePackageList(packages[manager]).length > 0);
}

function PackageFields({ value, onChange }: { value: PackageDraft; onChange: (value: PackageDraft) => void }) {
  const app = useApp();
  return (
    <details>
      <summary className="mut" style={{ cursor: "pointer" }}>{app.t("Packages · optional", "Packages · 可选")}</summary>
      <p className="mut">{app.t("One package requirement per line. Versions are allowed; command options are rejected by the server.", "每行一个 package，可带版本；服务端会拒绝命令行选项。")}</p>
      <div className="banner info"><span>ⓘ</span><span>{app.t(
        "Package lists are requirements for the Environment provider. A local runtime without package provisioning will reject the run instead of ignoring them.",
        "Package 列表是对 Environment Provider 的要求。若本地运行时不支持安装 Package，运行会明确失败，不会静默忽略。",
      )}</span></div>
      <div className="grid-2">
        {PACKAGE_MANAGERS.map((manager) => (
          <TextAreaField key={manager} label={manager} mono rows={3} value={value[manager]} onChange={(event) => onChange({ ...value, [manager]: event.target.value })} placeholder={manager === "npm" ? "typescript@5" : manager === "pip" ? "httpx==0.28" : ""} />
        ))}
      </div>
    </details>
  );
}

/** The environment's durable work-queue state (EnvRegistry + WorkQueue). A self-hosted
 * worker polls this queue; `depth` = items queued (backlog waiting to be claimed),
 * `pending` = items a worker has claimed and is processing. Polls alongside the env
 * list so a freshly-seeded healthcheck shows up. */
function EnvQueue({ id }: { id: string }) {
  const app = useApp();
  const stats = useQuery({
    queryKey: ["env-work-stats", id],
    queryFn: () => api.get<WorkQueueStats>(ws(`/v1/environments/${id}/work/stats`)),
    refetchInterval: 15_000,
    retry: false,
  });
  const s = stats.data;
  if (!s) return <span className="mut">—</span>;
  if (s.depth === 0 && s.pending === 0) return <Pill tone="neutral">{app.t("idle", "空闲")}</Pill>;
  const title = `${s.depth} queued · ${s.pending} in-flight · ${s.workers_polling} workers polling`;
  return (
    <Pill tone={s.pending > 0 ? "ok" : "info"} title={title}>
      {s.depth > 0 ? app.t(`${s.depth} queued`, `${s.depth} 排队`) : app.t(`${s.pending} active`, `${s.pending} 进行中`)}
    </Pill>
  );
}

function SandboxTiming({ id }: { id: string }) {
  const app = useApp();
  const binding = useQuery({
    queryKey: ["environment-sandbox-policy", id],
    queryFn: () => api.get<SandboxPolicyBinding>(ws(`/v1/awaken/environments/${id}/sandbox-execution-policy`)),
    retry: false,
  });
  const deferred = binding.data?.provisioning === "on_tool_use";
  return (
    <Pill tone={deferred ? "info" : "neutral"}>
      {deferred ? app.t("first Hand tool", "首次 Hand 工具") : app.t("eager", "预先创建")}
    </Pill>
  );
}

function CreateModal({ onClose }: { onClose: () => void }) {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [placement, setPlacement] = useState<"cloud" | "self_hosted">("cloud");
  const [net, setNet] = useState<"unrestricted" | "limited">("unrestricted");
  const [hosts, setHosts] = useState("");
  const [allowMcpServers, setAllowMcpServers] = useState(true);
  const [allowPackageManagers, setAllowPackageManagers] = useState(true);
  const [packages, setPackages] = useState<PackageDraft>(emptyPackages);
  const [provisioning, setProvisioning] = useState<SandboxProvisioning>("eager");
  const create = useMutation({
    mutationFn: async () => {
      const config = buildEnvironmentConfig(placement, net, hosts, packages, allowMcpServers, allowPackageManagers);
      assertSandboxProvisioningPlacement(placement, provisioning);
      const environment = await api.post<Environment>(ws("/v1/environments"), { name: name || "environment", description, config });
      const policy = buildDeferredSandboxPolicy(environment.id, placement, provisioning);
      if (policy) {
        const created = await api.post<SandboxExecutionPolicy>(ws("/v1/awaken/sandbox-execution-policies"), policy);
        await api.post<SandboxPolicyBinding>(
          ws(`/v1/awaken/environments/${environment.id}/sandbox-execution-policy`),
          { policy_id: created.id, version: created.version },
        );
      }
      return environment;
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["environments", workspace] });
      onClose();
    },
  });

  return (
    <Modal title={app.t("New environment", "新建运行环境")} onClose={onClose}>
        <TextField label={app.t("Name", "名称")} value={name} onChange={(e) => setName(e.target.value)} placeholder="claude-sandbox-github" />
        <TextField label={app.t("Description", "描述")} value={description} onChange={(e) => setDescription(e.target.value)} placeholder={app.t("What this Environment is for", "这个 Environment 的用途")} />

        <div className="field">
          <label>{app.t("Placement", "运行位置")}</label>
          <Segmented
            options={[
              { value: "cloud", label: app.t("cloud", "云托管") },
              { value: "self_hosted", label: app.t("self-hosted", "自管") },
            ]}
            value={placement}
            onChange={setPlacement}
          />
          <span className="mut">
            {placement === "cloud"
              ? app.t("Managed by your configured Awaken Agents deployment.", "由当前配置的 Awaken Agents 部署托管。")
              : app.t("Your own worker polls the work queue.", "你自己的 worker 拉取工作队列。")}
          </span>
        </div>

        {placement === "cloud" && (
            <div className="field">
              <label>{app.t("Networking", "网络")}</label>
              <Segmented
                options={[
                  { value: "unrestricted", label: app.t("unrestricted", "不限") },
                  { value: "limited", label: app.t("limited", "白名单") },
                ]}
                value={net}
                onChange={setNet}
              />
              {net === "limited" && (
                <>
                  <input className="input mono" placeholder="api.example.com, *.foo.com" value={hosts} onChange={(e) => setHosts(e.target.value)} />
                  <label className="row"><input type="checkbox" checked={allowMcpServers} onChange={(event) => setAllowMcpServers(event.target.checked)} />{app.t("Allow configured MCP servers", "允许已配置的 MCP Server")}</label>
                  <label className="row"><input type="checkbox" checked={allowPackageManagers} onChange={(event) => setAllowPackageManagers(event.target.checked)} />{app.t("Allow public package registries", "允许公共 Package Registry")}</label>
                </>
              )}
            </div>
        )}

        {placement === "cloud" && <PackageFields value={packages} onChange={setPackages} />}

        {placement === "self_hosted" && (
          <div className="field">
            <label>{app.t("Sandbox creation", "Sandbox 创建时机")}</label>
            <Segmented
              options={[
                { value: "eager", label: app.t("before first turn", "首次运行前") },
                { value: "on_tool_use", label: app.t("on first Hand tool", "首次 Hand 工具时") },
              ]}
              value={provisioning}
              onChange={setProvisioning}
            />
            <span className="mut">
              {provisioning === "on_tool_use"
                ? app.t(
                    "Native Awaken can answer with text, MCP, and instruction-only Skills without creating a Sandbox. The first filesystem, process, or sandbox-network tool waits for it to become ready.",
                    "原生 Awaken 可在不创建 Sandbox 的情况下完成文本、MCP 和纯指令 Skill；首次使用文件、进程或 Sandbox 网络工具时会等待其就绪。",
                  )
                : app.t(
                    "Create the Sandbox while the Session runtime is prepared.",
                    "在准备 Session 运行时创建 Sandbox。",
                  )}
            </span>
          </div>
        )}

        {create.error instanceof Error && <div className="err">{create.error.message}</div>}
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <Button onClick={onClose}>{app.t("Cancel", "取消")}</Button>
          <Button variant="primary" disabled={create.isPending} onClick={() => create.mutate()}>
            {app.t("Create", "创建")}
          </Button>
        </div>
    </Modal>
  );
}

function EditModal({ environment, onClose }: { environment: Environment; onClose: () => void }) {
  const app = useApp();
  const qc = useQueryClient();
  const [name, setName] = useState(environment.name);
  const [description, setDescription] = useState(environment.description ?? "");
  const [placement, setPlacement] = useState<"cloud" | "self_hosted">(environment.config.type);
  const initialNetworking = environment.config.networking;
  const [net, setNet] = useState<"unrestricted" | "limited">(initialNetworking?.type === "limited" ? "limited" : "unrestricted");
  const [hosts, setHosts] = useState(initialNetworking?.type === "limited" ? (initialNetworking.allowed_hosts ?? []).join(", ") : "");
  const [allowMcpServers, setAllowMcpServers] = useState(initialNetworking?.type === "limited" ? initialNetworking.allow_mcp_servers ?? false : true);
  const [allowPackageManagers, setAllowPackageManagers] = useState(initialNetworking?.type === "limited" ? initialNetworking.allow_package_managers ?? false : true);
  const [packages, setPackages] = useState<PackageDraft>(() => packageDraft(environment.config.packages));
  const update = useMutation({
    mutationFn: () => api.post<Environment>(ws(`/v1/environments/${environment.id}`), {
      name: name.trim() || environment.name,
      description,
      config: buildEnvironmentUpdateConfig(placement, net, hosts, packages, allowMcpServers, allowPackageManagers),
    }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["environments", app.workspaceId] });
      onClose();
    },
  });
  return (
    <Modal
      title={<>{app.t("Edit environment", "编辑运行环境")} · {environment.name}</>}
      onClose={onClose}
      width="min(820px, 94vw)"
      footer={<><Button onClick={onClose}>{app.t("Cancel", "取消")}</Button><Button variant="primary" disabled={update.isPending} onClick={() => update.mutate()}>{app.t("Save new revision", "保存新修订")}</Button></>}
    >
      <div className="stack">
        <div className="grid-2"><TextField label={app.t("Name", "名称")} value={name} onChange={(event) => setName(event.target.value)} /><TextField label={app.t("Description", "描述")} value={description} onChange={(event) => setDescription(event.target.value)} /></div>
        <div className="field"><label>{app.t("Placement", "运行位置")}</label><Segmented value={placement} onChange={setPlacement} options={[{ value: "cloud", label: app.t("cloud", "云托管") }, { value: "self_hosted", label: app.t("self-hosted", "自管") }]} /></div>
        {placement === "cloud" && <>
          <div className="field"><label>{app.t("Networking", "网络")}</label><Segmented value={net} onChange={setNet} options={[{ value: "unrestricted", label: app.t("unrestricted", "不限") }, { value: "limited", label: app.t("limited", "白名单") }]} />
          {net === "limited" && <><TextField label={app.t("Allowed hosts", "允许的 Host")} mono value={hosts} onChange={(event) => setHosts(event.target.value)} /><label className="row"><input type="checkbox" checked={allowMcpServers} onChange={(event) => setAllowMcpServers(event.target.checked)} />{app.t("Allow configured MCP servers", "允许已配置的 MCP Server")}</label><label className="row"><input type="checkbox" checked={allowPackageManagers} onChange={(event) => setAllowPackageManagers(event.target.checked)} />{app.t("Allow public package registries", "允许公共 Package Registry")}</label></>}</div>
          <PackageFields value={packages} onChange={setPackages} />
        </>}
        <div className="banner info"><span>ⓘ</span><span>{app.t("Saving creates a new immutable Environment revision. Existing Sessions keep their frozen revision.", "保存会生成新的不可变 Environment 修订；已有 Session 继续使用其冻结版本。")}</span></div>
        {update.error instanceof Error && <div className="err">{update.error.message}</div>}
      </div>
    </Modal>
  );
}

export default function EnvironmentsSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const confirm = useConfirm();
  const toast = useToast();
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<Environment | null>(null);
  const envs = useQuery({
    queryKey: ["environments", workspace],
    queryFn: () => api.get<Page<Environment>>(ws("/v1/environments")),
    refetchInterval: 30_000,
  });
  const archive = useMutation({
    mutationFn: (id: string) => api.post(ws(`/v1/environments/${id}/archive`)),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["environments", workspace] });
      toast.ok(app.t("Environment archived.", "运行环境已归档。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const archiveEnvironment = async (id: string) => {
    const approved = await confirm({
      title: app.t("Archive this environment?", "归档该运行环境？"),
      body: app.t("New sessions can no longer select it. Existing sessions are not deleted.", "新会话将不能再选择它；已有会话不会被删除。"),
      confirmLabel: app.t("Archive", "归档"),
    });
    if (approved) archive.mutate(id);
  };
  const rows = envs.data?.data ?? [];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span />
        <Button variant="primary" onClick={() => setCreating(true)}>
          + {app.t("New environment", "新建环境")}
        </Button>
      </div>
      {envs.error instanceof Error && <div className="err">{envs.error.message}</div>}
      <Card className="responsive-table-card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Environment", "运行环境")}</th>
              <th>{app.t("Placement", "运行位置")}</th>
              <th>{app.t("Networking", "网络")}</th>
              <th>{app.t("Sandbox creation", "Sandbox 创建")}</th>
              <th>{app.t("Queue", "队列")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((e) => (
              <tr key={e.id}>
                <td data-label={app.t("Environment", "运行环境")}>
                  <strong>{e.name}</strong>
                  <TechnicalId value={e.id} />
                </td>
                <td data-label={app.t("Placement", "运行位置")}>
                  <Pill tone={e.config.type === "self_hosted" ? "agent" : "neutral"}>
                    {e.config.type === "self_hosted" ? app.t("self-hosted", "自管") : app.t("cloud", "云托管")}
                  </Pill>
                </td>
                <td className="mut" data-label={app.t("Networking", "网络")}>{networkingLabel(e.config, app.locale === "zh")}</td>
                <td data-label={app.t("Sandbox creation", "Sandbox 创建")}><SandboxTiming id={e.id} /></td>
                <td data-label={app.t("Queue", "队列")}>{e.archived_at ? <span className="mut">—</span> : <EnvQueue id={e.id} />}</td>
                <td className="responsive-table-actions" style={{ textAlign: "right" }}>
                  {e.id !== "env_local" && <Button variant="ghost" onClick={() => setEditing(e)}>{app.t("Edit", "编辑")}</Button>}
                  {e.archived_at ? (
                    <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>
                  ) : (
                    <Button
                      variant="ghost"
                      disabled={archive.isPending && archive.variables === e.id}
                      onClick={() => void archiveEnvironment(e.id)}
                    >
                      {archive.isPending && archive.variables === e.id ? app.t("Archiving…", "正在归档…") : app.t("Archive", "归档")}
                    </Button>
                  )}
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={6} className="mut">
                  {envs.isLoading ? "…" : app.t("No Environments yet. Create one when placement, packages, network, or Sandbox policy differs from the local default.", "还没有 Environment。运行位置、软件包、网络或 Sandbox 策略不同于本地默认值时，请创建一个。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>
      {creating && <CreateModal onClose={() => setCreating(false)} />}
      {editing && <EditModal environment={editing} onClose={() => setEditing(null)} />}
    </>
  );
}

export function buildDeferredSandboxPolicy(
  environmentId: string,
  placement: "cloud" | "self_hosted",
  provisioning: SandboxProvisioning,
): Pick<SandboxExecutionPolicy, "id" | "config" | "provisioning" | "disabled"> | null {
  if (provisioning !== "on_tool_use") return null;
  assertSandboxProvisioningPlacement(placement, provisioning);
  return {
    id: `environment-${environmentId}-sandbox`,
    config: {},
    provisioning: "on_tool_use",
    disabled: false,
  };
}

function assertSandboxProvisioningPlacement(
  placement: "cloud" | "self_hosted",
  provisioning: SandboxProvisioning,
): void {
  if (provisioning === "on_tool_use" && placement !== "self_hosted") {
    throw new Error("on_tool_use Sandbox provisioning requires a self-hosted native Awaken Environment");
  }
}

export function isolationLabel(config: EnvironmentConfig): string {
  return config.networking?.type ?? "provider default";
}

export function networkingLabel(config: EnvironmentConfig, zh = false): string {
  const value = isolationLabel(config);
  if (!zh) return value === "provider default" ? "Provider default" : value === "unrestricted" ? "Unrestricted" : "Allowlist";
  return value === "provider default" ? "由 Provider 决定" : value === "unrestricted" ? "不限" : "白名单";
}

export function buildEnvironmentConfig(
  placement: "cloud" | "self_hosted",
  networking: "unrestricted" | "limited",
  hosts: string,
  packages: PackageDraft = emptyPackages(),
  allowMcpServers = true,
  allowPackageManagers = true,
): EnvironmentConfig {
  if (placement === "self_hosted") return { type: "self_hosted" };
  const config: EnvironmentConfig = {
    type: "cloud",
    networking: networking === "limited"
      ? {
          type: "limited",
          allowed_hosts: hosts.split(",").map((host) => host.trim()).filter(Boolean),
          allow_mcp_servers: allowMcpServers,
          allow_package_managers: allowPackageManagers,
        }
      : { type: "unrestricted" },
  };
  if (hasPackageRequirements(packages)) {
    config.packages = {
      type: "packages",
      ...Object.fromEntries(PACKAGE_MANAGERS.map((manager) => [manager, parsePackageList(packages[manager])])),
    };
  }
  return config;
}

/** Updates must send an explicit null when the user clears every package field.
 * Omitting the field means "preserve the previous package requirements" in the
 * Managed Agents PATCH contract. */
export function buildEnvironmentUpdateConfig(
  placement: "cloud" | "self_hosted",
  networking: "unrestricted" | "limited",
  hosts: string,
  packages: PackageDraft = emptyPackages(),
  allowMcpServers = true,
  allowPackageManagers = true,
): EnvironmentUpdateConfig {
  const config = buildEnvironmentConfig(placement, networking, hosts, packages, allowMcpServers, allowPackageManagers);
  if (placement === "cloud" && !hasPackageRequirements(packages)) return { ...config, packages: null };
  return config;
}
