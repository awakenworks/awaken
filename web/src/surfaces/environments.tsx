// Workspace · Environments: the reusable execution templates sessions run in
// (Managed Agents `/v1/environments`). A session references one by
// `environment_id`. cloud = Anthropic-hosted; self_hosted = your own worker.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api, ws } from "../lib/api/client";
import type {
  Environment,
  EnvironmentConfig,
  Page,
  SandboxExecutionPolicy,
  SandboxPolicyBinding,
  SandboxProvisioning,
  WorkQueueStats,
} from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Button, Card, Modal, Pill, Segmented, TextField } from "../components/ui";

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
  const [placement, setPlacement] = useState<"cloud" | "self_hosted">("cloud");
  const [net, setNet] = useState<"unrestricted" | "limited">("unrestricted");
  const [hosts, setHosts] = useState("");
  const [provisioning, setProvisioning] = useState<SandboxProvisioning>("eager");
  const create = useMutation({
    mutationFn: async () => {
      const config = buildEnvironmentConfig(placement, net, hosts);
      assertSandboxProvisioningPlacement(placement, provisioning);
      const environment = await api.post<Environment>(ws("/v1/environments"), { name: name || "environment", config });
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
              ? app.t("Anthropic-hosted container.", "Anthropic 托管容器。")
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
                <input className="input mono" placeholder="api.example.com, *.foo.com" value={hosts} onChange={(e) => setHosts(e.target.value)} />
              )}
            </div>
        )}

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

export default function EnvironmentsSurface() {
  const app = useApp();
  const workspace = app.workspaceId;
  const qc = useQueryClient();
  const [creating, setCreating] = useState(false);
  const envs = useQuery({
    queryKey: ["environments", workspace],
    queryFn: () => api.get<Page<Environment>>(ws("/v1/environments")),
    refetchInterval: 30_000,
  });
  const archive = useMutation({
    mutationFn: (id: string) => api.post(ws(`/v1/environments/${id}/archive`)),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["environments", workspace] }),
  });
  const rows = envs.data?.data ?? [];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut">
          {app.t(
            "Reusable container templates. Sessions reference one by environment_id.",
            "可复用的容器模板。会话按 environment_id 引用。",
          )}
        </span>
        <Button variant="primary" onClick={() => setCreating(true)}>
          + {app.t("New environment", "新建环境")}
        </Button>
      </div>
      {envs.error instanceof Error && <div className="err">{envs.error.message}</div>}
      <Card style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>Environment</th>
              <th>{app.t("Name", "名称")}</th>
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
                <td className="mono">{e.id}</td>
                <td>{e.name}</td>
                <td>
                  <Pill tone={e.config.type === "self_hosted" ? "agent" : "neutral"}>
                    {e.config.type}
                  </Pill>
                </td>
                <td className="mut">{isolationLabel(e.config)}</td>
                <td><SandboxTiming id={e.id} /></td>
                <td>{e.archived_at ? <span className="mut">—</span> : <EnvQueue id={e.id} />}</td>
                <td style={{ textAlign: "right" }}>
                  {e.archived_at ? (
                    <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>
                  ) : (
                    <Button
                      variant="ghost"
                      style={{ height: 22 }}
                      disabled={archive.isPending}
                      onClick={() => archive.mutate(e.id)}
                    >
                      {app.t("Archive", "归档")}
                    </Button>
                  )}
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={7} className="mut">
                  {envs.isLoading ? "…" : app.t("No environments yet.", "还没有运行环境。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>
      {creating && <CreateModal onClose={() => setCreating(false)} />}
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

export function buildEnvironmentConfig(
  placement: "cloud" | "self_hosted",
  networking: "unrestricted" | "limited",
  hosts: string,
): EnvironmentConfig {
  if (placement === "self_hosted") return { type: "self_hosted" };
  return {
    type: "cloud",
    networking: networking === "limited"
      ? {
          type: "limited",
          allowed_hosts: hosts.split(",").map((host) => host.trim()).filter(Boolean),
          allow_mcp_servers: true,
        }
      : { type: "unrestricted" },
  };
}
