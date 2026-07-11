// Project · Deployments: schedule an agent to run on a cron
// (Managed Agents `/v1/deployments`). Each firing creates a session from the
// deployment's initial_events, running the agent in its environment.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Button, Card, Pill, SelectField, TextAreaField, TextField } from "../components/ui";
import { api } from "../lib/api/client";
import type { AgentConfigList, Deployment, Environment, Page } from "../lib/api/types";
import { useApp } from "../lib/app-state";

function CreateModal({ onClose }: { onClose: () => void }) {
  const app = useApp();
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [agentId, setAgentId] = useState("");
  const [envId, setEnvId] = useState("");
  const [expression, setExpression] = useState("0 20 * * 5");
  const [timezone, setTimezone] = useState("UTC");
  const [message, setMessage] = useState("Run the scheduled task.");

  // Published config agents are the deployable set — the same source the console
  // authors against (/v1/config/agents), not the managed registry projection.
  const agents = useQuery({
    queryKey: ["config-agents"],
    queryFn: () => api.get<AgentConfigList>("/v1/config/agents"),
  });
  const envs = useQuery({
    queryKey: ["environments"],
    queryFn: () => api.get<Page<Environment>>("/v1/environments"),
  });

  const create = useMutation({
    mutationFn: () =>
      api.post<Deployment>("/v1/deployments", {
        name: name || "deployment",
        agent: agentId,
        environment_id: envId,
        schedule: { type: "cron", expression, timezone },
        initial_events: [{ type: "user.message", content: [{ type: "text", text: message }] }],
      }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["deployments"] });
      onClose();
    },
  });

  return (
    <div className="overlay" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h3>{app.t("New deployment", "新建部署")}</h3>
        <TextField
          label={app.t("Name", "名称")}
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="nightly-report"
        />
        <SelectField label={app.t("Agent", "智能体")} value={agentId} onChange={(e) => setAgentId(e.target.value)}>
          <option value="">{app.t("Select an agent…", "选择智能体…")}</option>
          {(agents.data?.data ?? [])
            .filter((a) => a.published)
            .map((a) => (
              <option key={a.id} value={a.id}>
                {a.name ? `${a.name} (${a.id})` : a.id}
              </option>
            ))}
        </SelectField>
        <SelectField label={app.t("Environment", "运行环境")} value={envId} onChange={(e) => setEnvId(e.target.value)}>
          <option value="">{app.t("Select an environment…", "选择运行环境…")}</option>
          {(envs.data?.data ?? []).map((v) => (
            <option key={v.id} value={v.id}>
              {v.name} ({v.id})
            </option>
          ))}
        </SelectField>
        <div className="field">
          <label>{app.t("Cron expression", "Cron 表达式")}</label>
          <input className="input mono" value={expression} onChange={(e) => setExpression(e.target.value)} placeholder="0 20 * * 5" />
          <span className="mut">{app.t("Standard cron; each firing creates a session.", "标准 cron;每次触发创建一个会话。")}</span>
        </div>
        <TextField
          label={app.t("Timezone", "时区")}
          mono
          value={timezone}
          onChange={(e) => setTimezone(e.target.value)}
          placeholder="UTC"
        />
        <TextAreaField
          label={app.t("Kickoff message", "启动消息")}
          rows={3}
          value={message}
          onChange={(e) => setMessage(e.target.value)}
        />
        {create.error instanceof Error && <div className="err">{create.error.message}</div>}
        <div className="row" style={{ justifyContent: "flex-end" }}>
          <Button onClick={onClose}>
            {app.t("Cancel", "取消")}
          </Button>
          <Button variant="primary" disabled={create.isPending || !agentId || !envId} onClick={() => create.mutate()}>
            {app.t("Create", "创建")}
          </Button>
        </div>
      </div>
    </div>
  );
}

export default function DeploymentsSurface() {
  const app = useApp();
  const qc = useQueryClient();
  const [creating, setCreating] = useState(false);
  const deployments = useQuery({
    queryKey: ["deployments"],
    queryFn: () => api.get<Page<Deployment>>("/v1/deployments"),
    refetchInterval: 30_000,
  });
  const act = useMutation({
    mutationFn: ({ id, action }: { id: string; action: string }) => api.post(`/v1/deployments/${id}/${action}`),
    onSuccess: () => void qc.invalidateQueries({ queryKey: ["deployments"] }),
  });
  const rows = deployments.data?.data ?? [];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span className="mut">
          {app.t(
            "A deployment runs an agent on a cron schedule; each firing creates a session.",
            "部署按 cron 计划运行智能体;每次触发创建一个会话。",
          )}
        </span>
        <Button variant="primary" onClick={() => setCreating(true)}>
          + {app.t("New deployment", "新建部署")}
        </Button>
      </div>
      {deployments.error instanceof Error && <div className="err">{deployments.error.message}</div>}
      <Card style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>Deployment</th>
              <th>{app.t("Name", "名称")}</th>
              <th>{app.t("Schedule", "计划")}</th>
              <th>{app.t("Next run", "下次运行")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((d) => (
              <tr key={d.id}>
                <td className="mono">{d.id}</td>
                <td>{d.name}</td>
                <td className="mut mono">
                  {d.schedule.expression} · {d.schedule.timezone}
                </td>
                <td className="mut">{d.schedule.upcoming_runs_at?.[0] ?? "—"}</td>
                <td style={{ textAlign: "right" }}>
                  {d.archived_at ? (
                    <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>
                  ) : (
                    <div className="row" style={{ justifyContent: "flex-end" }}>
                      <Button
                        variant="primary"
                        style={{ height: 22 }}
                        disabled={act.isPending}
                        onClick={() => act.mutate({ id: d.id, action: "run" })}
                      >
                        {app.t("Run", "运行")}
                      </Button>
                      <Button
                        variant="ghost"
                        style={{ height: 22 }}
                        disabled={act.isPending}
                        onClick={() => act.mutate({ id: d.id, action: d.paused_reason ? "unpause" : "pause" })}
                      >
                        {d.paused_reason ? app.t("Unpause", "恢复") : app.t("Pause", "暂停")}
                      </Button>
                      <Button
                        variant="ghost"
                        style={{ height: 22 }}
                        disabled={act.isPending}
                        onClick={() => act.mutate({ id: d.id, action: "archive" })}
                      >
                        {app.t("Archive", "归档")}
                      </Button>
                    </div>
                  )}
                </td>
              </tr>
            ))}
            {rows.length === 0 && (
              <tr>
                <td colSpan={5} className="mut">
                  {deployments.isLoading ? "…" : app.t("No deployments yet.", "还没有部署。")}
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
