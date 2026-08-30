// Session detail: header actions (rename/archive/interrupt) + an
// agent/properties aside around the shared TranscriptView. This surface owns
// exactly one live SessionLog; header, chat, approvals, and trace consume it.

import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { managedSessionPresentationPhase } from "@awaken/managed-session-projection";
import { useState } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { TranscriptView } from "../components/session/Transcript";
import TraceView from "../components/session/TraceView";
import SessionFiles from "../components/session/SessionFiles";
import SessionIntegrations from "../components/session/SessionIntegrations";
import SessionThreads from "../components/session/SessionThreads";
import { Button, Card, Modal, Pill, Segmented, TechnicalId, TextField, useConfirm, useToast } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { Environment, Page, Session, SessionAgent } from "../lib/api/types";
import { mcpToolsetPolicySummary, type McpToolsetPolicySummary } from "../lib/agent-toolsets";
import { useApp } from "../lib/app-state";
import { sessionDisplayTitle } from "../lib/presentation";
import { sessionErrorText } from "../lib/session-log";
import { useSessionLog } from "../lib/useSessionLog";

/** The agent's model can arrive as a bare id or a `{ id }` object — coerce to text. */
function modelText(m: unknown): string {
  if (typeof m === "string") return m;
  if (m && typeof m === "object" && "id" in m) return String((m as { id: unknown }).id);
  return "";
}

/** Runtime is a derived execution coordinate, never caller-authored metadata.
 * Managed model ids preserve an ACP/A2A backend even when the Session API keeps
 * metadata empty, so the UI must not mislabel those runs as native. */
export function sessionRuntime(session: Session | undefined): string | null {
  const model = modelText(session?.agent.model);
  const acp = model.match(/^(acp:[^@/;]+)/)?.[1]
    ?? model.match(/(?:^|;)executor=(acp:[^;]+)/)?.[1];
  if (acp) return acp;
  if (model.startsWith("a2a:")) return "A2A remote";
  return session ? "native" : null;
}

export function sessionEnvironmentName(
  environmentId: string | null | undefined,
  environments: Environment[] | undefined,
  defaultLabel: string,
): string {
  if (!environmentId) return defaultLabel;
  return environments?.find((environment) => environment.id === environmentId)?.name || environmentId;
}

interface MonetaryAmount {
  amount: string;
  currency: string;
}

/** Managed Agents currently prices in USD minor units. Keep the wire integer as
 * text until display so a cost never gains binary floating-point rounding. */
export function managedMoneyLabel(value: MonetaryAmount | null | undefined): string | null {
  if (!value || !/^-?\d+$/.test(value.amount)) return null;
  const negative = value.amount.startsWith("-");
  const digits = negative ? value.amount.slice(1) : value.amount;
  const padded = digits.padStart(3, "0");
  const major = padded.slice(0, -2).replace(/^0+(?=\d)/, "");
  const minor = padded.slice(-2);
  return `${value.currency} ${negative ? "-" : ""}${major}.${minor}`;
}

export function outcomeTone(result: string): "ok" | "warn" | "danger" | "neutral" {
  if (result === "satisfied") return "ok";
  if (result === "failed" || result === "max_iterations_reached") return "danger";
  if (result === "running" || result === "evaluating" || result === "needs_revision") return "warn";
  return "neutral";
}

export interface SessionMcpPolicy extends McpToolsetPolicySummary {
  name: string;
}

export interface SessionConfigDestination {
  kind: "instructions" | "tools" | "integrations" | "knowledge" | "orchestration";
  href: string;
  count?: number;
}

/** Route runtime evidence back to the one configuration aggregate that owns it.
 * These links intentionally target the current draft: the Managed Session
 * snapshot remains the authority for this run, while edits can only affect a
 * later publication and new Sessions. */
export function sessionConfigDestinations(
  workspaceId: string,
  agent: SessionAgent | undefined,
): SessionConfigDestination[] {
  if (!agent?.id) return [];
  const root = `/w/${encodeURIComponent(workspaceId)}/agents/${encodeURIComponent(agent.id)}`;
  const destinations: SessionConfigDestination[] = [
    { kind: "instructions", href: `${root}?stage=build&section=instructions` },
  ];
  if ((agent.tools?.length ?? 0) > 0) {
    destinations.push({ kind: "tools", href: `${root}?stage=build&section=tools`, count: agent.tools?.length });
  }
  if ((agent.mcp_servers?.length ?? 0) > 0) {
    destinations.push({ kind: "integrations", href: `${root}?stage=build&section=integrations`, count: agent.mcp_servers?.length });
  }
  if ((agent.skills?.length ?? 0) > 0) {
    destinations.push({ kind: "knowledge", href: `${root}?stage=build&section=knowledge`, count: agent.skills?.length });
  }
  if ((agent.multiagent?.agents?.length ?? 0) > 0) {
    destinations.push({ kind: "orchestration", href: `${root}?stage=advanced&section=orchestration`, count: agent.multiagent?.agents?.length });
  }
  return destinations;
}

/** Project the immutable Agent snapshot already returned by the Managed Session
 * API. This is read-only execution evidence, not another configuration source. */
export function sessionMcpPolicies(agent: SessionAgent | undefined): SessionMcpPolicy[] {
  return (agent?.mcp_servers ?? []).map((server) => ({
    name: server.name,
    ...mcpToolsetPolicySummary(agent?.tools ?? [], server.name),
  }));
}

const SESSION_VIEWS = ["chat", "collaboration", "inputs", "artifacts", "integrations", "trace"] as const;
type SessionView = typeof SESSION_VIEWS[number];

export function sessionViewFromSearch(view: string | null, event: string | null): SessionView {
  if (event) return "trace";
  return SESSION_VIEWS.includes(view as SessionView) ? view as SessionView : "chat";
}

export default function SessionDetailSurface() {
  const app = useApp();
  const { ws: wsId = "default", sid = "" } = useParams();
  const [searchParams, setSearchParams] = useSearchParams();
  const fromQuickstart = searchParams.get("from") === "quickstart";
  const qc = useQueryClient();
  const confirm = useConfirm();
  const toast = useToast();
  // Workspace-scoped via ws() (tenancy is an edge aspect); flat under default scope.
  const base = ws(`/v1/sessions/${sid}`);
  const eventsKey = ["session-events", wsId, sid];
  const selectedEventId = searchParams.get("event")?.trim() || undefined;
  const view = sessionViewFromSearch(searchParams.get("view"), selectedEventId ?? null);
  const setView = (next: SessionView) => {
    const params = new URLSearchParams(searchParams);
    if (next === "chat") params.delete("view"); else params.set("view", next);
    if (next !== "trace") params.delete("event");
    setSearchParams(params, { replace: true });
  };
  const [controlResult, setControlResult] = useState<string | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [titleDraft, setTitleDraft] = useState("");

  const session = useQuery({
    queryKey: ["session", wsId, sid],
    queryFn: () => api.get<Session>(base),
    refetchInterval: 15_000,
  });
  const environments = useQuery({
    queryKey: ["environments", wsId],
    queryFn: () => api.get<Page<Environment>>(ws("/v1/environments")),
    staleTime: 30_000,
  });
  // One hook owns merge/reducer/admission, SSE, mutation identity, and pending
  // transport state for every detail control and view.
  const sessionLog = useSessionLog(base, eventsKey, {
    sessionStatus: session.data?.status,
    live: true,
    followLive: true,
  });
  const runtime = sessionLog.runtime;
  const admission = sessionLog.admission;
  const effectiveStatus = managedSessionPresentationPhase(runtime, session.data?.status);
  const runtimeName = sessionRuntime(session.data);
  const mcpPolicies = sessionMcpPolicies(session.data?.agent);
  const configDestinations = sessionConfigDestinations(wsId, session.data?.agent);
  const environmentName = sessionEnvironmentName(
    session.data?.environment_id,
    environments.data?.data,
    app.t("Default", "默认"),
  );
  const outcomes = session.data?.outcome_evaluations ?? [];
  const budgetLabel = managedMoneyLabel(session.data?.budget?.max_list_cost);
  const costLabel = managedMoneyLabel(session.data?.usage?.list_cost);
  const inputTokens = session.data?.usage?.input_tokens;
  const outputTokens = session.data?.usage?.output_tokens;
  const hasUsageEvidence = budgetLabel != null || costLabel != null
    || inputTokens != null || outputTokens != null || outcomes.length > 0;
  // A committed tool request waiting for an external resolution is recoverable
  // work. Inputs and tool replies that are currently resolving are ordinary
  // active turns, so presenting their interrupt as "Recover run" falsely tells
  // the operator that healthy work is stuck.
  const needsRecovery = runtime.pendingToolIds.size > 0;

  const rename = useMutation({
    mutationFn: (title: string) => api.post<Session>(base, { title }),
    onSuccess: (s) => {
      qc.setQueryData(["session", wsId, sid], s);
      void qc.invalidateQueries({ queryKey: ["sessions", wsId] });
      setRenaming(false);
      toast.ok(app.t("Session renamed.", "会话已重命名。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const archive = useMutation({
    mutationFn: () => api.post<Session>(`${base}/archive`),
    onSuccess: (s) => {
      qc.setQueryData(["session", wsId, sid], s);
      void qc.invalidateQueries({ queryKey: ["sessions", wsId] });
      toast.ok(app.t("Session archived.", "会话已归档。"));
    },
    onError: (cause) => toast.err(cause instanceof Error ? cause.message : String(cause)),
  });
  const archiveSession = async () => {
    const approved = await confirm({
      title: app.t("Archive this session?", "归档该会话？"),
      body: app.t("It remains readable, but no new work should be sent to it.", "它仍可读取，但不应再向其发送新任务。"),
      confirmLabel: app.t("Archive", "归档"),
    });
    if (approved) archive.mutate();
  };
  const interruptSession = async () => {
    const approved = await confirm({
      title: needsRecovery
        ? app.t("Recover this stuck run?", "恢复这个卡住的运行？")
        : app.t("Stop the current run?", "停止当前运行？"),
      body: needsRecovery
        ? app.t("The committed pending tool requests are interrupted through the Session event stream. Conversation, repository changes, and history remain available, and a new message can be sent after the terminal event is projected.", "将通过 Session 事件流中断已提交的待处理工具请求；对话、仓库修改和历史都会保留，终止事件投影后即可发送新消息。")
        : app.t("The current model turn is interrupted. The conversation and completed work remain available, and you can send a new message afterwards.", "当前模型回合会被中断；对话与已完成工作仍会保留，之后可以继续发送新消息。"),
      confirmLabel: needsRecovery
        ? app.t("Recover run", "恢复运行")
        : app.t("Stop run", "停止运行"),
      danger: true,
    });
    if (!approved) return;
    setControlResult(null);
    try {
      const result = await sessionLog.send([{ type: "user.interrupt" }]);
      const receipt = result.data?.at(-1);
      setControlResult(receipt ? `${receipt.type} accepted · ${receipt.id}` : null);
      void session.refetch();
    } catch {
      // The shared hook owns and presents the exact transport/projection error.
    }
  };

  return (
    <>
      <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
        <div style={{ minWidth: 0 }}>
          <Link to={`/w/${wsId}/sessions`}>‹ {app.t("Sessions", "会话")}</Link>
          <div className="row" style={{ marginTop: 6 }}>
            <h2 style={{ margin: 0, fontSize: 20 }}>
              {sessionDisplayTitle(session.data?.title, session.data?.agent.id, app.locale)}
            </h2>
            {session.data?.archived_at && (
              <Pill tone="neutral">
                {app.t("archived", "已归档")}
              </Pill>
            )}
          </div>
          <TechnicalId value={sid} />
        </div>
        <span className="row">
          <Button
            variant="ghost"
            onClick={() => {
              setTitleDraft(session.data?.title ?? "");
              setRenaming(true);
            }}
          >
            ✎ {app.t("Rename", "重命名")}
          </Button>
          {!session.data?.archived_at && (
            <Button variant="ghost" disabled={archive.isPending} onClick={() => void archiveSession()}>
              ⌫ {archive.isPending ? app.t("Archiving…", "正在归档…") : app.t("Archive", "归档")}
            </Button>
          )}
          <Button variant="danger" disabled={!admission.canInterrupt || sessionLog.sendPending} onClick={() => void interruptSession()}>
            ⏹ {needsRecovery ? app.t("Recover run", "恢复运行") : app.t("Stop run", "停止运行")}
          </Button>
        </span>
      </div>

      {renaming && (
        <Modal title={app.t("Rename session", "重命名会话")} onClose={() => setRenaming(false)}>
          <TextField label={app.t("Title", "标题")} value={titleDraft} onChange={(event) => setTitleDraft(event.target.value)} />
          {rename.error instanceof Error && <div className="err">{rename.error.message}</div>}
          <div className="row" style={{ justifyContent: "flex-end" }}>
            <Button onClick={() => setRenaming(false)}>{app.t("Cancel", "取消")}</Button>
            <Button variant="primary" disabled={rename.isPending} onClick={() => rename.mutate(titleDraft)}>
              {rename.isPending ? app.t("Saving…", "正在保存…") : app.t("Save", "保存")}
            </Button>
          </div>
        </Modal>
      )}

      {controlResult && <div className="banner info">{controlResult}</div>}
      {fromQuickstart && (
        <div className="banner info">
          {app.t(
            "Your Agent is published and this durable Session is ready. Send a message here, then connect your application with the SDK example in API & protocols.",
            "Agent 已发布，这个持久 Session 已就绪。先在此发送消息，再通过 API 与协议页的 SDK 示例连接你的应用。",
          )}{" "}
          <Link to={`/w/${wsId}/protocols`}>{app.t("Open SDK example", "查看 SDK 示例")}</Link>
        </div>
      )}
      {sessionLog.sendError && <div className="banner err">{sessionLog.sendError.message}</div>}
      {view !== "chat" && sessionLog.projectionError && (
        <div className="banner err">
          {app.t(
            "Committed Session history is inconsistent; input is disabled until the projection is reloaded.",
            "已提交的会话历史不一致；重新加载投影前将禁用输入。",
          )}
        </div>
      )}

      <div className="session-detail-layout" style={{ display: "flex", gap: 14, alignItems: "flex-start" }}>
        <div style={{ flex: 1.8, minWidth: 0 }}>
          {session.error instanceof Error && <div className="err">{session.error.message}</div>}
          <Segmented
            className="segmented"
            options={[
              { value: "chat", label: app.t("Chat", "对话") },
              { value: "collaboration", label: app.t("Child runs", "子运行") },
              { value: "inputs", label: app.t("Inputs", "输入") },
              { value: "artifacts", label: app.t("Artifacts", "产物") },
              { value: "integrations", label: app.t("Integrations", "集成") },
              { value: "trace", label: app.t("Trace", "追踪") },
            ]}
            value={view}
            onChange={setView}
          />
          {view === "chat" && <TranscriptView key={sid} sessionLog={sessionLog} />}
          {view === "collaboration" && <SessionThreads base={base} workspaceId={wsId} />}
          {view === "inputs" && <SessionFiles base={base} sid={sid} view="inputs" />}
          {view === "artifacts" && <SessionFiles base={base} sid={sid} view="artifacts" />}
          {view === "integrations" && <SessionIntegrations session={session.data} workspaceId={wsId} />}
          {view === "trace" && (
            <TraceView
              log={sessionLog.log}
              loadError={sessionLog.projectionError ? null : sessionLog.loadError}
              selectedEventId={selectedEventId}
            />
          )}
        </div>

        <aside className="session-detail-aside" style={{ width: 300, flex: "none", display: "flex", flexDirection: "column", gap: 12 }}>
          <Card style={{ padding: "12px 14px" }}>
            <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
              {app.t("Effective configuration", "本次生效配置")}
            </h2>
            {session.data ? (
              <>
                <div className="row">
                  <Pill tone="agent" title={session.data.agent.id}>{session.data.agent.name || session.data.agent.id}</Pill>
                  {session.data.agent.version != null && (
                    <Pill tone="neutral">{app.t("revision", "修订")} {session.data.agent.version}</Pill>
                  )}
                  {modelText(session.data.agent.model) && <code>{modelText(session.data.agent.model)}</code>}
                </div>
                <div className="mut" style={{ marginTop: 8, fontSize: 12 }}>
                  {app.t("Tools", "工具")} {session.data.agent.tools?.length ?? 0} · Skills {session.data.agent.skills?.length ?? 0} · MCP {session.data.agent.mcp_servers?.length ?? 0}
                </div>
                <p className="hint" style={{ margin: "10px 0 0" }}>
                  {app.t(
                    "This immutable snapshot is pinned to this Session. The links below open the current Agent draft; changes affect only future Sessions after publication.",
                    "这是固定到本次 Session 的不可变快照。下方链接打开当前 Agent 草稿；修改只有重新发布后才会影响新的 Session。",
                  )}
                </p>
                <nav className="session-config-links" aria-label={app.t("Open current Agent configuration", "打开当前 Agent 配置")}>
                  {configDestinations.map((destination) => {
                    const labels = {
                      instructions: app.t("Model & instructions", "模型与指令"),
                      tools: app.t("Tools & permissions", "工具与权限"),
                      integrations: app.t("MCP integrations", "MCP 集成"),
                      knowledge: app.t("Skills & knowledge", "技能与知识"),
                      orchestration: app.t("Orchestration", "编排"),
                    };
                    return (
                      <Link key={destination.kind} to={destination.href}>
                        {labels[destination.kind]}{destination.count == null ? "" : ` · ${destination.count}`} →
                      </Link>
                    );
                  })}
                </nav>
                {mcpPolicies.length > 0 && (
                  <div style={{ borderTop: "1px solid var(--line)", marginTop: 10, paddingTop: 10 }}>
                    <strong style={{ fontSize: 12 }}>
                      {app.t("Effective MCP policy", "实际 MCP 策略")}
                    </strong>
                    <div className="mut" style={{ fontSize: 11, marginTop: 3 }}>
                      {app.t("Frozen for this Session", "已固化到本次会话")}
                    </div>
                    {mcpPolicies.map((policy) => (
                      <div className="row" key={policy.name} style={{ marginTop: 7, gap: 5 }}>
                        <strong style={{ fontSize: 12 }}>{policy.name}</strong>
                        <Pill tone={policy.enabled ? "ok" : "neutral"}>
                          {policy.enabled ? app.t("enabled", "已启用") : app.t("disabled", "已停用")}
                        </Pill>
                        <Pill tone={policy.permission === "always_allow" ? "ok" : "warn"}>
                          {policy.permission === "always_allow"
                            ? app.t("allow by default", "默认允许")
                            : app.t("ask by default", "默认询问")}
                        </Pill>
                        {policy.namedOverrides > 0 && (
                          <span className="mut" style={{ fontSize: 11 }}>
                            {policy.namedOverrides} {app.t("overrides", "项覆盖")}
                          </span>
                        )}
                      </div>
                    ))}
                  </div>
                )}
              </>
            ) : (
              <span className="mut">…</span>
            )}
          </Card>
          <Card style={{ padding: "12px 14px" }}>
            <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
              {app.t("Properties", "属性")}
            </h2>
            <div className="mut" style={{ fontSize: 12, display: "flex", flexDirection: "column", gap: 4 }}>
              <span>{app.t("Created", "创建时间")} {session.data?.created_at ? new Date(session.data.created_at).toLocaleString() : "—"}</span>
              <span>{app.t("Status", "状态")} {effectiveStatus === "running" ? app.t("running", "运行中") : effectiveStatus === "idle" ? app.t("idle", "空闲") : effectiveStatus ?? "—"}</span>
              <span>{app.t("Pending tools", "待审批工具")} {runtime.pendingToolIds.size}</span>
              <span>{app.t("Resolving tools", "处理中工具")} {runtime.resolvingToolIds.size}</span>
              <span>{app.t("Resolving inputs", "处理中输入")} {runtime.resolvingInputIds.size}</span>
              <span>{app.t("Can send message", "允许发送消息")} {admission.canSendMessage ? app.t("yes", "是") : app.t("no", "否")}</span>
              {runtime.latestError && <span className="err">{app.t("Last error", "最近错误")} {sessionErrorText(runtime.latestError)}</span>}
              <span>
                {app.t("Environment", "运行环境")} <strong title={session.data?.environment_id ?? undefined}>{environmentName}</strong>
              </span>
              {/* Runtime provenance: which backend actually executed this run (native vs an
                  ACP CLI), read off the session metadata the environment stamped at create. */}
              <span className="row" style={{ gap: 6, alignItems: "center" }}>
                {app.t("Runtime", "运行时")}
                <Pill tone={runtimeName && runtimeName !== "native" ? "agent" : "neutral"}>
                  {runtimeName === "native" ? app.t("Awaken native", "Awaken 原生") : runtimeName ?? "—"}
                </Pill>
              </span>
            </div>
          </Card>
          {hasUsageEvidence && (
            <Card style={{ padding: "12px 14px" }}>
              <h2 style={{ fontSize: 12, textTransform: "uppercase", letterSpacing: ".06em", color: "var(--fg3)" }}>
                {app.t("Outcome & usage", "结果与用量")}
              </h2>
              <div className="mut" style={{ fontSize: 12, display: "grid", gap: 5 }}>
                {budgetLabel && <span>{app.t("Cost limit", "费用上限")} <strong>{budgetLabel}</strong></span>}
                {costLabel && <span>{app.t("Tracked cost", "累计费用")} <strong>{costLabel}</strong></span>}
                {(inputTokens != null || outputTokens != null) && (
                  <span>
                    {app.t("Tokens", "Token")} <strong>{inputTokens ?? 0}</strong> {app.t("in", "输入")} · <strong>{outputTokens ?? 0}</strong> {app.t("out", "输出")}
                  </span>
                )}
              </div>
              {outcomes.length > 0 && (
                <div className="session-outcome-list" aria-label={app.t("Defined outcomes", "已定义结果")}>
                  {outcomes.map((outcome) => (
                    <div key={outcome.outcome_id}>
                      <div className="row" style={{ justifyContent: "space-between", alignItems: "flex-start" }}>
                        <strong>{outcome.description}</strong>
                        <Pill tone={outcomeTone(outcome.result)}>{outcome.result.replaceAll("_", " ")}</Pill>
                      </div>
                      {outcome.explanation && <p className="hint">{outcome.explanation}</p>}
                      <TechnicalId value={outcome.outcome_id} />
                    </div>
                  ))}
                </div>
              )}
            </Card>
          )}
        </aside>
      </div>
    </>
  );
}
