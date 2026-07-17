// Project · Environments: the reusable container templates sessions run in
// (Managed Agents `/v1/environments`). A session references one by
// `environment_id`. cloud = Anthropic-hosted; self_hosted = your own worker.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { api } from "../lib/api/client";
import type { Environment, EnvironmentConfig, Page, SandboxConfig, WorkQueueStats } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { Button, Card, Pill, Segmented, Switch, TextField } from "../components/ui";
import { useCapabilities } from "../lib/useCapabilities";
import { SandboxEditor } from "../components/environment/SandboxEditor";

/** The environment's durable work-queue state (EnvRegistry + WorkQueue). A self-hosted
 * worker polls this queue; `depth` = items queued (backlog waiting to be claimed),
 * `pending` = items a worker has claimed and is processing. Polls alongside the env
 * list so a freshly-seeded healthcheck shows up. */
function EnvQueue({ id }: { id: string }) {
  const app = useApp();
  const stats = useQuery({
    queryKey: ["env-work-stats", id],
    queryFn: () => api.get<WorkQueueStats>(`/v1/environments/${id}/work/stats`),
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

function CreateModal({ onClose }: { onClose: () => void }) {
  const app = useApp();
  const qc = useQueryClient();
  const caps = useCapabilities();
  const runtimes = caps.data?.runtimes ?? [{ id: "awaken", label: "Native", kind: "native" as const, description: "" }];
  const sandboxCap = caps.data?.sandbox;

  const [name, setName] = useState("");
  const [placement, setPlacement] = useState<"cloud" | "self_hosted">("cloud");
  const [runtime, setRuntime] = useState("awaken");
  const [net, setNet] = useState<"unrestricted" | "limited">("unrestricted");
  const [hosts, setHosts] = useState("");
  const [sandboxOn, setSandboxOn] = useState(false);
  // Seed the sandbox from the first preset so the toggle yields a valid spec immediately.
  const [sandbox, setSandbox] = useState<SandboxConfig>({});
  const runtimeInfo = runtimes.find((r) => r.id === runtime);

  function toggleSandbox(on: boolean) {
    setSandboxOn(on);
    if (on && Object.keys(sandbox).length === 0 && sandboxCap?.presets[0]) {
      setSandbox(sandboxCap.presets[0].spec);
    }
  }

  const create = useMutation({
    mutationFn: () => {
      const config: EnvironmentConfig = { type: placement };
      if (runtime !== "awaken") config.runtime = runtime; // absent = native
      if (sandboxOn && sandboxCap) config.sandbox = sandbox;
      // Cloud egress only applies when NOT sandboxed (a sandbox owns its own egress).
      if (placement === "cloud" && !sandboxOn) {
        config.networking =
          net === "limited"
            ? { type: "limited", allowed_hosts: hosts.split(",").map((h) => h.trim()).filter(Boolean), allow_mcp_servers: true }
            : { type: "unrestricted" };
      }
      return api.post<Environment>("/v1/environments", { name: name || "environment", config });
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["environments"] });
      onClose();
    },
  });

  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>{app.t("New environment", "新建运行环境")}</h3>
        <TextField label={app.t("Name", "名称")} value={name} onChange={(e) => setName(e.target.value)} placeholder="claude-sandbox-github" />

        <div className="field">
          <label>{app.t("Runtime", "运行时")}</label>
          <Segmented options={runtimes.map((r) => ({ value: r.id, label: r.label }))} value={runtime} onChange={setRuntime} />
          {runtimeInfo?.description && <span className="mut">{runtimeInfo.description}</span>}
        </div>

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

        {sandboxCap && (
          <label className="row" style={{ gap: 8, alignItems: "center" }}>
            <Switch checked={sandboxOn} onChange={(e) => toggleSandbox(e.target.checked)} />
            <span>{app.t("Run in an isolated sandbox (bwrap)", "在隔离沙箱中运行(bwrap)")}</span>
          </label>
        )}

        {sandboxOn && sandboxCap ? (
          <SandboxEditor value={sandbox} onChange={setSandbox} sandbox={sandboxCap} />
        ) : (
          placement === "cloud" && (
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
          )
        )}

        {create.error instanceof Error && <div className="err">{create.error.message}</div>}
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <Button onClick={onClose}>{app.t("Cancel", "取消")}</Button>
          <Button variant="primary" disabled={create.isPending} onClick={() => create.mutate()}>
            {app.t("Create", "创建")}
          </Button>
        </div>
      </div>
    </div>
  );
}

export default function EnvironmentsSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const [creating, setCreating] = useState(false);
  const envs = useQuery({
    queryKey: ["environments"],
    queryFn: () => api.get<Page<Environment>>("/v1/environments"),
    refetchInterval: 30_000,
  });
  const archive = useMutation({
    mutationFn: (id: string) => api.post(`/v1/environments/${id}/archive`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["environments"] }),
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
              <th>{app.t("Runtime", "运行时")}</th>
              <th>{app.t("Networking", "网络")}</th>
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
                <td className="mut">{e.config.networking?.type ?? "—"}</td>
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
                <td colSpan={6} className="mut">
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
