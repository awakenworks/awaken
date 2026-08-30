// Workspace · Deployments: schedule an Agent to run on a cron
// (Managed Agents `/v1/deployments`). Each firing creates a session from the
// deployment's initial_events, running the agent in its environment.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useParams } from "react-router";
import { Button, Card, Modal, Pill, SelectField, TechnicalId, TextAreaField, TextField, useConfirm, useToast } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { AgentConfigList, CursorPage, Deployment, DeploymentRun, Environment, Page } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { visibleAgents } from "../lib/visible-agents";
import { dateTimeLabel, entityDisplayName, identifierLabel } from "../lib/presentation";

const WEEKDAYS = {
  en: ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"],
  zh: ["周日", "周一", "周二", "周三", "周四", "周五", "周六"],
} as const;

function cronTime(hour: number, minute: number, locale: "en" | "zh"): string {
  return new Intl.DateTimeFormat(locale === "zh" ? "zh-CN" : "en-US", {
    hour: "numeric",
    minute: "2-digit",
    hour12: locale === "en",
    timeZone: "UTC",
  }).format(new Date(Date.UTC(2026, 0, 1, hour, minute)));
}

/** Present the common schedules Awaken creates without hiding their exact cron.
 * Unusual expressions remain explicitly identified as cron instead of being
 * guessed into an inaccurate promise. */
export function cronScheduleLabel(expression: string, locale: "en" | "zh"): string {
  const fields = expression.trim().split(/\s+/);
  if (fields.length !== 5) return locale === "zh" ? "自定义 Cron 计划" : "Custom cron schedule";
  const [minuteText, hourText, dayOfMonth, month, dayOfWeek] = fields;
  const minute = Number(minuteText);
  const hour = Number(hourText);
  if (minuteText.startsWith("*/") && hourText === "*" && dayOfMonth === "*" && month === "*" && dayOfWeek === "*") {
    const interval = Number(minuteText.slice(2));
    if (Number.isInteger(interval) && interval > 0) {
      return locale === "zh" ? `每 ${interval} 分钟` : `Every ${interval} minutes`;
    }
  }
  if (!Number.isInteger(minute) || minute < 0 || minute > 59 || !Number.isInteger(hour) || hour < 0 || hour > 23) {
    return locale === "zh" ? "自定义 Cron 计划" : "Custom cron schedule";
  }
  if (dayOfMonth !== "*" || month !== "*") return locale === "zh" ? "自定义 Cron 计划" : "Custom cron schedule";
  const time = cronTime(hour, minute, locale);
  if (dayOfWeek === "*") return locale === "zh" ? `每天 ${time}` : `Every day at ${time}`;
  if (dayOfWeek === "1-5") return locale === "zh" ? `工作日 ${time}` : `Weekdays at ${time}`;
  const weekday = Number(dayOfWeek) === 7 ? 0 : Number(dayOfWeek);
  if (Number.isInteger(weekday) && weekday >= 0 && weekday <= 6) {
    return locale === "zh"
      ? `每${WEEKDAYS.zh[weekday]} ${time}`
      : `Every ${WEEKDAYS.en[weekday]} at ${time}`;
  }
  return locale === "zh" ? "自定义 Cron 计划" : "Custom cron schedule";
}

export function deploymentRunPresentation(run: DeploymentRun): "failed" | "session_created" | "starting" {
  if (run.error) return "failed";
  if (run.session_id) return "session_created";
  return "starting";
}

function CreateModal({ onClose }: { onClose: () => void }) {
  const app = useApp();
  const workspace = app.workspaceId;
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
    queryKey: ["config-agents", workspace],
    queryFn: () => api.get<AgentConfigList>(ws("/v1/config/agents")),
  });
  const envs = useQuery({
    queryKey: ["environments", workspace],
    queryFn: () => api.get<Page<Environment>>(ws("/v1/environments")),
  });

  const create = useMutation({
    mutationFn: () =>
      api.post<Deployment>(ws("/v1/deployments"), {
        name: name || "deployment",
        agent: agentId,
        environment_id: envId,
        schedule: { type: "cron", expression, timezone },
        initial_events: [{ type: "user.message", content: [{ type: "text", text: message }] }],
      }),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ["deployments", workspace] });
      onClose();
    },
  });

  return (
    <Modal title={app.t("New deployment", "新建部署")} onClose={onClose}>
        <TextField
          label={app.t("Name", "名称")}
          value={name}
          onChange={(e) => setName(e.target.value)}
          placeholder="nightly-report"
        />
        <SelectField label={app.t("Agent", "智能体")} value={agentId} onChange={(e) => setAgentId(e.target.value)}>
          <option value="">{app.t("Select an agent…", "选择智能体…")}</option>
          {visibleAgents(agents.data?.data)
            .filter((a) => a.published)
            .map((a) => (
              <option key={a.id} value={a.id}>
                {entityDisplayName(a.name, identifierLabel(a.id))}
              </option>
            ))}
        </SelectField>
        <SelectField label={app.t("Environment", "运行环境")} value={envId} onChange={(e) => setEnvId(e.target.value)}>
          <option value="">{app.t("Select an environment…", "选择运行环境…")}</option>
          {(envs.data?.data ?? []).map((v) => (
            <option key={v.id} value={v.id}>
              {entityDisplayName(v.name, identifierLabel(v.id))}
            </option>
          ))}
        </SelectField>
        <div className="field">
          <label>{app.t("Cron expression", "Cron 表达式")}</label>
          <input className="input mono" value={expression} onChange={(e) => setExpression(e.target.value)} placeholder="0 20 * * 5" />
          <span className="mut">
            {cronScheduleLabel(expression, app.locale)} · {app.t("Each firing creates a separate Session.", "每次触发都会创建独立会话。")}
          </span>
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
    </Modal>
  );
}

export default function DeploymentsSurface() {
  const app = useApp();
  const { ws: wsId = "default" } = useParams();
  const qc = useQueryClient();
  const confirm = useConfirm();
  const toast = useToast();
  const [creating, setCreating] = useState(false);
  const [lastRun, setLastRun] = useState<DeploymentRun | null>(null);
  const deployments = useQuery({
    queryKey: ["deployments", wsId],
    queryFn: () => api.get<Page<Deployment>>(ws("/v1/deployments")),
    refetchInterval: 30_000,
  });
  const runs = useQuery({
    queryKey: ["deployment-runs", wsId],
    queryFn: () => api.get<CursorPage<DeploymentRun>>(ws("/v1/deployment_runs")),
    refetchInterval: 15_000,
  });
  const act = useMutation({
    mutationFn: ({ id, action }: { id: string; action: string }) =>
      api.post<Deployment | DeploymentRun>(ws(`/v1/deployments/${id}/${action}`)),
    onSuccess: (result, variables) => {
      if (variables.action === "run") setLastRun(result as DeploymentRun);
      void qc.invalidateQueries({ queryKey: ["deployments", wsId] });
      void qc.invalidateQueries({ queryKey: ["deployment-runs", wsId] });
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const runAction = async (id: string, action: string) => {
    if (action === "archive") {
      const approved = await confirm({
        title: app.t("Archive this deployment?", "归档该部署？"),
        body: app.t("Its schedule will stop. Existing runs and sessions remain available.", "它的计划将停止；已有运行和会话仍会保留。"),
        confirmLabel: app.t("Archive", "归档"),
      });
      if (!approved) return;
    }
    act.mutate({ id, action });
  };
  const rows = deployments.data?.data ?? [];

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between" }}>
        <span />
        <Button variant="primary" onClick={() => setCreating(true)}>
          + {app.t("New deployment", "新建部署")}
        </Button>
      </div>
      {deployments.error instanceof Error && <div className="err">{deployments.error.message}</div>}
      {lastRun && (
        <div className="banner info">
          <span>✓</span>
          <span>
            {app.t("Deployment run created.", "部署运行已创建。")}
            {lastRun.session_id ? <>
              {" "}<Link to={`/w/${wsId}/sessions/${lastRun.session_id}`}>{app.t("Open Session", "打开会话")}</Link>
            </> : null}
            <TechnicalId value={[lastRun.id, lastRun.session_id].filter(Boolean).join(" · ")} />
            {lastRun.error ? <span className="err"> · {lastRun.error.message ?? lastRun.error.type}</span> : null}
          </span>
        </div>
      )}
      <Card className="responsive-table-card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Deployment", "部署")}</th>
              <th>{app.t("Schedule", "计划")}</th>
              <th>{app.t("Next run", "下次运行")}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {rows.map((d) => (
              <tr key={d.id}>
                <td data-label={app.t("Deployment", "部署")}><strong>{entityDisplayName(d.name, identifierLabel(d.id))}</strong><TechnicalId value={d.id} /></td>
                <td data-label={app.t("Schedule", "计划")}>
                  <strong>{cronScheduleLabel(d.schedule.expression, app.locale)}</strong>
                  <div className="mut" style={{ fontSize: 12, marginTop: 3 }}>
                    {d.schedule.timezone} · <code>{d.schedule.expression}</code>
                  </div>
                </td>
                <td data-label={app.t("Next run", "下次运行")} className="mut">{dateTimeLabel(d.schedule.upcoming_runs_at?.[0], app.locale)}</td>
                <td className="responsive-table-actions" style={{ textAlign: "right" }}>
                  {d.archived_at ? (
                    <Pill tone="neutral">{app.t("archived", "已归档")}</Pill>
                  ) : (
                    <div className="row" style={{ justifyContent: "flex-end" }}>
                      <Button
                        variant="primary"
                        style={{ height: 22 }}
                        disabled={act.isPending}
                        onClick={() => void runAction(d.id, "run")}
                      >
                        {app.t("Run once", "立即运行")}
                      </Button>
                      <Button
                        variant="ghost"
                        style={{ height: 22 }}
                        disabled={act.isPending}
                        onClick={() => void runAction(d.id, d.paused_reason ? "unpause" : "pause")}
                      >
                        {d.paused_reason ? app.t("Unpause", "恢复") : app.t("Pause", "暂停")}
                      </Button>
                      <Button
                        variant="ghost"
                        style={{ height: 22 }}
                        disabled={act.isPending}
                        onClick={() => void runAction(d.id, "archive")}
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
                <td colSpan={4} className="mut">
                  {deployments.isLoading ? "…" : app.t("No Deployments yet. Create one after its Agent has completed a successful Session.", "还没有 Deployment。请先让 Agent 完成一次成功会话，再创建持续运行任务。")}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </Card>
      <div style={{ marginTop: 18 }}>
        <h2 className="section-title">{app.t("Recent deployment runs", "最近部署运行")}</h2>
        <p className="hint">{app.t(
          "Every scheduled or manual trigger remains visible and links to the Session it created.",
          "每次计划或手动触发都会保留，并可打开它创建的 Session。",
        )}</p>
      </div>
      {runs.error instanceof Error && <div className="err">{runs.error.message}</div>}
      <Card className="responsive-table-card" style={{ padding: 0 }}>
        <table className="table">
          <thead>
            <tr>
              <th>{app.t("Deployment", "部署")}</th>
              <th>{app.t("Trigger", "触发方式")}</th>
              <th>{app.t("Result", "结果")}</th>
              <th>{app.t("Created", "创建时间")}</th>
            </tr>
          </thead>
          <tbody>
            {(runs.data?.data ?? []).map((run) => {
              const deployment = rows.find((candidate) => candidate.id === run.deployment_id);
              const result = deploymentRunPresentation(run);
              return (
                <tr key={run.id}>
                  <td data-label={app.t("Deployment", "部署")}>
                    <strong>{entityDisplayName(deployment?.name, identifierLabel(run.deployment_id))}</strong>
                    <TechnicalId value={run.id} />
                  </td>
                  <td data-label={app.t("Trigger", "触发方式")}>
                    <Pill tone="neutral">
                      {run.trigger_context?.type === "schedule"
                        ? app.t("scheduled", "计划触发")
                        : app.t("manual", "手动触发")}
                    </Pill>
                  </td>
                  <td data-label={app.t("Result", "结果")}>
                    {result === "failed" ? (
                      <span><Pill tone="danger">{app.t("failed", "失败")}</Pill><span className="err"> {run.error?.message ?? run.error?.type}</span></span>
                    ) : result === "session_created" ? (
                      <span className="row">
                        <Pill tone="ok">{app.t("Session created", "已创建 Session")}</Pill>
                        <Link to={`/w/${wsId}/sessions/${run.session_id}`}>{app.t("Open Session →", "打开 Session →")}</Link>
                      </span>
                    ) : (
                      <Pill tone="warn">{app.t("starting", "启动中")}</Pill>
                    )}
                  </td>
                  <td data-label={app.t("Created", "创建时间")} className="mut">{dateTimeLabel(run.created_at, app.locale)}</td>
                </tr>
              );
            })}
            {(runs.data?.data.length ?? 0) === 0 && (
              <tr><td colSpan={4} className="mut">{runs.isLoading ? "…" : app.t("No Deployment runs yet. Use Run once to verify the first result before relying on the schedule.", "还没有 Deployment Run。请先“立即运行”验证首个结果，再依赖计划调度。")}</td></tr>
            )}
          </tbody>
        </table>
      </Card>
      {creating && <CreateModal onClose={() => setCreating(false)} />}
    </>
  );
}
