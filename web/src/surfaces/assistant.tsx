// Admin Assistant: an in-console copilot that authors agents in plain English. It is a
// chat against the built-in `__admin_assistant` agent (same <Transcript> engine as the
// session detail and editor Sandbox). Its authoring tools PERSIST each draft as an
// unpublished config agent, so the loop closes here: describe → it drafts → a status card
// with Open-in-editor → review → publish. Reused by the full page AND the global FAB;
// when opened over an agent editor it targets that agent (patch), not a fresh draft.

import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useLocation, useNavigate, useParams } from "react-router";
import Transcript from "../components/session/Transcript";
import { Button, Card, Pill, Skeleton } from "../components/ui";
import { assistantContextForLocation, type AssistantSurfaceContext } from "../lib/assistant-guidance";
import { api, IdempotencyScope, ws } from "../lib/api/client";
import { presentApiProblem } from "../lib/api-problem";
import type { AgentConfig, Session } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useGate } from "../lib/useGate";
import { useModels } from "../lib/useModels";

const ASSISTANT_ID = "__admin_assistant";
const DRAFT_TOOLS = ["admin_draft_agent", "admin_patch_agent"];

interface EventLike {
  type: string;
  name?: string;
  input?: { id?: string };
}

/** A rich status card for one drafted/patched agent — reads the persisted config so it
 * shows exactly what the assistant authored (tools, overrides, plugins), with a deep
 * link into the editor. Beats a wall of raw tool-call JSON. */
function DraftCard({ id, wsId }: { id: string; wsId: string }) {
  const app = useApp();
  const nav = useNavigate();
  const cfg = useQuery({
    queryKey: ["draft-card", wsId, id],
    queryFn: () => api.get<AgentConfig>(ws(`/v1/config/agents/${id}`)),
    refetchInterval: 4000,
    retry: false,
  });
  const c = cfg.data;
  const tools = c?.tools?.length ?? 0;
  const overrides = c?.tool_overrides?.length ?? 0;
  const plugins = c?.plugins ?? [];
  // `published` rides on the config-plane response but isn't on the authored type.
  const unpublished = c ? !(c as { published?: unknown }).published : false;
  return (
    <Card style={{ padding: "10px 12px", display: "flex", alignItems: "center", gap: 10 }}>
      <Pill tone="agent">{id}</Pill>
      <span className="mut" style={{ fontSize: 12 }}>
        {tools} {app.t("tools", "工具")}
        {overrides > 0 && ` · ${overrides} ${app.t("overrides", "覆盖")}`}
        {plugins.length > 0 && ` · ${plugins.join(", ")}`}
        {unpublished && ` · ${app.t("draft", "草稿")}`}
      </span>
      <Button
        variant="ghost"
        style={{ marginLeft: "auto", height: 26 }}
        onClick={() => nav(`/w/${wsId}/agents/${id}`)}
      >
        {app.t("Open in editor →", "在编辑器打开 →")}
      </Button>
    </Card>
  );
}

/** The agents this session drafted so far, read from its own tool calls. */
function DraftedAgents({ base, wsId }: { base: string; wsId: string }) {
  const app = useApp();
  const events = useQuery({
    queryKey: ["assistant-drafts", base],
    queryFn: () => api.get<{ data: EventLike[] }>(`${base}/events`),
    refetchInterval: 3000,
  });
  const ids = Array.from(
    new Set(
      (events.data?.data ?? [])
        .filter((e) => e.type === "agent.tool_use" && e.name && DRAFT_TOOLS.includes(e.name))
        .map((e) => e.input?.id)
        .filter((id): id is string => !!id),
    ),
  );
  if (ids.length === 0) return null;
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 6, marginTop: 10 }}>
      <span className="mut" style={{ fontSize: 11 }}>
        {app.t("Drafted this session — review & publish:", "本次起草——去审阅并发布:")}
      </span>
      {ids.map((id) => (
        <DraftCard key={id} id={id} wsId={wsId} />
      ))}
    </div>
  );
}

/** No credentialed model → the assistant can't run. Say so and point to Models. */
function NoModel({ wsId }: { wsId: string }) {
  const app = useApp();
  const nav = useNavigate();
  return (
    <div className="banner gate assistant-prerequisite">
      <span>ⓘ</span>
      <span className="assistant-prerequisite-copy">
        {app.t(
          "The assistant needs a model to run on. Configure a provider + credential first.",
          "助手需要一个模型才能运行。请先配置一个 provider + 凭证。",
        )}
      </span>
      <div className="assistant-prerequisite-actions">
        <Button variant="ghost" onClick={() => nav(`/w/${wsId}/models`)}>
          {app.t("Go to Models →", "去 Models →")}
        </Button>
      </div>
    </div>
  );
}

function AssistantGuide({
  context,
  targetAgentId,
  disabled,
  onPrompt,
}: {
  context: AssistantSurfaceContext;
  targetAgentId?: string;
  disabled: boolean;
  onPrompt: (text: string) => void;
}) {
  const app = useApp();
  return (
    <div className="assistant-guide">
      <div className="assistant-scope">
        <span>
          <strong>{app.t("Ask any Console question", "询问任何 Console 问题")}</strong>
          <small>{app.t("The current page and Workspace are included automatically.", "系统会自动带上当前页面和工作区上下文。")}</small>
        </span>
        <span>
          <strong>{app.t("Complete supported setup", "完成受支持的配置")}</strong>
          <small>{app.t("Draft or refine Agents and create Environments from plain language.", "可用自然语言起草或修改 Agent，并创建运行环境。")}</small>
        </span>
        <span>
          <strong>{app.t("Review before activation", "生效前人工审阅")}</strong>
          <small>{app.t("The Assistant never publishes Agents or reveals stored secrets.", "助手不会发布 Agent，也不会读取已保存的秘密。")}</small>
        </span>
      </div>
      <div className="assistant-current-context">
        <span className="mut">{app.t("Current context", "当前上下文")}</span>
        <Pill tone="agent">{targetAgentId || context.label}</Pill>
        {targetAgentId && <span className="mut">{app.t("changes apply to this draft", "修改将应用到此草稿")}</span>}
      </div>
      <div className="assistant-suggestions" role="list" aria-label={app.t("Suggested questions", "建议问题")}>
        {context.suggestions.map((suggestion) => (
          <span role="listitem" key={suggestion}>
            <Button variant="ghost" disabled={disabled} onClick={() => onPrompt(suggestion)}>
              {suggestion}
            </Button>
          </span>
        ))}
      </div>
    </div>
  );
}

/** The reusable chat panel. `targetAgentId` steers changes to that exact draft;
 * `surfaceContext` supplies the current route, help topic, and starter questions. */
export function AssistantPanel({
  wsId,
  targetAgentId,
  surfaceContext,
  autoMessage,
  onAgentChanged,
  onRunSettled,
}: {
  wsId: string;
  targetAgentId?: string;
  surfaceContext: AssistantSurfaceContext;
  autoMessage?: { id: string; text: string };
  onAgentChanged?: (id: string, paths: string[]) => void;
  onRunSettled?: () => void;
}) {
  const app = useApp();
  const gate = useGate(`/v1/agents/${ASSISTANT_ID}`);
  const models = useModels();
  const [sid, setSid] = useState<string | null>(null);
  const [promptRequest, setPromptRequest] = useState<{ id: string; text: string }>();
  const started = useRef(false);
  const ensureAttempted = useRef(false);
  const createIdentity = useRef(new IdempotencyScope("assistant-session-create"));
  const ensureAssistant = useMutation({
    mutationFn: () => api.post<{ status: string }>(ws("/v1/config/agents/__admin_assistant/ensure")),
    onSuccess: () => gate.refetch(),
  });
  const start = useMutation({
    mutationFn: () => {
      const request = {
      agent: ASSISTANT_ID,
      title: `Assistant · ${targetAgentId || surfaceContext.label}`,
      };
      return api.post<Session>(ws("/v1/sessions"), request, createIdentity.current.headersFor(request));
    },
    onSuccess: (s) => {
      createIdentity.current.complete();
      setSid(s.id);
    },
  });
  useEffect(() => {
    if (!started.current && models.ready.length > 0 && gate.status === "live") {
      started.current = true;
      start.mutate();
    }
  }, [start, models.ready.length, gate.status]);
  useEffect(() => {
    if (gate.status !== "absent" || models.ready.length === 0 || ensureAttempted.current) return;
    ensureAttempted.current = true;
    ensureAssistant.mutate();
  }, [ensureAssistant, gate.status, models.ready.length]);

  const contextPrefix = targetAgentId
    ? `[Console context: workspace=${wsId}; route=${surfaceContext.path}; current Agent draft=${targetAgentId}; help topic=${surfaceContext.topic}. Refine only this Agent with admin_patch_agent when the operator requests a change. For questions, explain before proposing changes.]`
    : `[Console context: workspace=${wsId}; route=${surfaceContext.path}; surface=${surfaceContext.label}; help topic=${surfaceContext.topic}. Answer questions using admin_explain_console. Execute only supported Agent or Environment authoring tools; otherwise give exact UI steps and state the boundary.]`;
  const placeholder = targetAgentId
    ? app.t(`Ask about or change ${targetAgentId}…`, `询问或修改 ${targetAgentId}…`)
    : app.t("Ask a question or describe what you want to accomplish…", "提问，或描述你想完成的事情…");
  const assistantReady = !models.loading && models.ready.length > 0 && gate.status === "live";
  const ensureProblem = ensureAssistant.error
    ? presentApiProblem(ensureAssistant.error)
    : undefined;
  const ensureProblemCopy = ensureProblem && (() => {
    switch (ensureProblem.action) {
      case "authenticate":
        return app.t("Your session expired. Sign in again, then retry.", "登录状态已过期。请重新登录后重试。");
      case "authorize":
        return app.t("This Workspace does not authorize the requested Assistant action.", "当前工作区未授权所需的助手操作。");
      case "refresh_conflict":
        return ensureProblem.code === "assistant_model_unavailable"
          ? app.t("No usable model is published in this Workspace.", "当前工作区没有可用的已发布模型。")
          : app.t("Assistant publication conflicts with the current Workspace state. Refresh before retrying.", "助手发布与当前工作区状态冲突。请刷新后再重试。");
      case "retry_dependency":
        return app.t("An Assistant dependency is temporarily unavailable. Retry is safe.", "助手依赖暂时不可用，可以安全重试。");
      default:
        return app.t("The Assistant could not be prepared. Inspect the error details.", "助手准备失败，请检查错误详情。");
    }
  })();
  const guide = (
    <AssistantGuide
      context={surfaceContext}
      targetAgentId={targetAgentId}
      disabled={!assistantReady}
      onPrompt={(text) => setPromptRequest({ id: `${Date.now()}-${text}`, text })}
    />
  );

  if (models.loading || gate.status === "loading") return <>{guide}<Skeleton height={80} /></>;
  if (models.ready.length === 0) return <>{guide}<NoModel wsId={wsId} /></>;
  if (gate.status === "absent") return (
    <>
      {guide}
      <div className="banner gate assistant-prerequisite">
        <span>{ensureAssistant.isPending ? "◔" : "◌"}</span>
        <span className="assistant-prerequisite-copy">
          {app.t(
            ensureAssistant.isPending
              ? "Preparing the Assistant with the verified Workspace model…"
              : ensureProblemCopy ?? "The Assistant could not be prepared.",
            ensureAssistant.isPending
              ? "正在使用工作区已验证模型准备助手…"
              : ensureProblemCopy ?? "助手准备失败。",
          )}
          {ensureAssistant.error instanceof Error && <small className="err">{ensureAssistant.error.message}</small>}
          {ensureProblem?.requestId && <small className="mut">Correlation ID: {ensureProblem.requestId}</small>}
        </span>
        <div className="assistant-prerequisite-actions">
          <Button
            variant="ghost"
            disabled={ensureAssistant.isPending}
            onClick={() => {
              ensureAttempted.current = true;
              ensureAssistant.mutate();
            }}
          >{app.t("Retry", "重试")}</Button>
          <Button variant="ghost" onClick={() => window.location.assign(`/w/${wsId}/models`)}>{app.t("Open Models", "打开模型")}</Button>
        </div>
      </div>
    </>
  );
  if (gate.status === "error") return <>{guide}<div className="err">{gate.error.message}</div></>;

  return (
    <>
      {guide}
      {!sid ? (
          start.error instanceof Error ? (
            <div className="banner err">
              <span>{start.error.message}</span>
              <Button variant="ghost" disabled={start.isPending} onClick={() => start.mutate()}>
                {start.isPending ? app.t("Retrying…", "正在重试…") : app.t("Try again", "重试")}
              </Button>
            </div>
          ) : <Skeleton height={80} />
        ) : (
          <>
            <div className="assistant-context">
              <span className="readiness-icon ready">✓</span>
              <span>
                <strong>{app.t("Using existing Workspace configuration", "正在使用现有工作区配置")}</strong>
                <small>
                  {app.t(
                    `${models.ready.length} runnable model choices are available. The Assistant selects from published, ready capabilities and never copies credentials.`,
                    `已有 ${models.ready.length} 个可运行模型。助手只从已发布且就绪的能力中选择，绝不复制凭证。`,
                  )}
                </small>
              </span>
            </div>
            <Transcript
              base={ws(`/v1/sessions/${sid}`)}
              queryKey={["assistant-events", sid]}
              placeholder={placeholder}
              contextPrefix={contextPrefix}
              autoMessage={promptRequest ?? autoMessage}
              onRunSettled={onRunSettled}
              onToolComplete={(tool, result) => {
                if (tool.type !== "agent.tool_use" || !("name" in tool) || !DRAFT_TOOLS.includes(String(tool.name))) return;
                if ("is_error" in result && result.is_error === true) return;
                const input = "input" in tool
                  ? tool.input as { id?: unknown; patch?: Record<string, unknown> }
                  : undefined;
                if (typeof input?.id === "string") {
                  const authored = input.patch ?? (input as Record<string, unknown>);
                  const aliases: Record<string, string> = {
                    instructions: "system",
                    tool_ids: "tools",
                    plugin_config: "plugin_config",
                    resources: "resources",
                  };
                  const paths = Object.keys(authored)
                    .filter((key) => key !== "id")
                    .flatMap((key) => {
                      if (key === "plugin_config" && authored[key] && typeof authored[key] === "object") {
                        return Object.keys(authored[key] as Record<string, unknown>).map((plugin) => `plugin_config.${plugin}`);
                      }
                      return [aliases[key] ?? key];
                    });
                  onAgentChanged?.(input.id, paths);
                }
              }}
            />
            <DraftedAgents base={ws(`/v1/sessions/${sid}`)} wsId={wsId} />
          </>
        )}
    </>
  );
}

export default function AssistantSurface() {
  const { ws: wsId = "default" } = useParams();
  const location = useLocation();
  const surfaceContext = assistantContextForLocation(location.pathname, location.search);
  return (
    <Card className="assistant-page-card">
      <AssistantPanel wsId={wsId} surfaceContext={surfaceContext} />
    </Card>
  );
}
