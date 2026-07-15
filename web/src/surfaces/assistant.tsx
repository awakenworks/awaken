// Admin Assistant: an in-console copilot that authors agents in plain English. It is a
// chat against the built-in `__admin_assistant` agent (same <Transcript> engine as the
// session detail and editor Sandbox). Its authoring tools PERSIST each draft as an
// unpublished config agent, so the loop closes here: describe → it drafts → a status card
// with Open-in-editor → review → publish. Reused by the full page AND the global FAB;
// when opened over an agent editor it targets that agent (patch), not a fresh draft.

import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router";
import Gate from "../components/app/Gate";
import Transcript from "../components/session/Transcript";
import { Button, Card, Pill, Skeleton } from "../components/ui";
import { api, ws } from "../lib/api/client";
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
    queryKey: ["draft-card", id],
    queryFn: () => api.get<AgentConfig>(`/v1/config/agents/${id}`),
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
    <div className="banner gate">
      <span>ⓘ</span>
      <span>
        {app.t(
          "The assistant needs a model to run on. Configure a provider + credential first.",
          "助手需要一个模型才能运行。请先配置一个 provider + 凭证。",
        )}
      </span>
      <Button variant="ghost" style={{ marginLeft: "auto", height: 26 }} onClick={() => nav(`/w/${wsId}/models`)}>
        {app.t("Go to Models →", "去 Models →")}
      </Button>
    </div>
  );
}

/** The reusable chat panel. `targetAgentId` (set when opened over an agent editor) steers
 * the assistant to refine THAT agent (patch) instead of drafting a fresh one. */
export function AssistantPanel({ wsId, targetAgentId }: { wsId: string; targetAgentId?: string }) {
  const app = useApp();
  const gate = useGate(`/v1/agents/${ASSISTANT_ID}`);
  const models = useModels();
  const [sid, setSid] = useState<string | null>(null);
  const started = useRef(false);
  const start = useMutation({
    mutationFn: () => api.post<Session>(ws("/v1/sessions"), { agent: ASSISTANT_ID, title: "admin assistant" }),
    onSuccess: (s) => setSid(s.id),
  });
  useEffect(() => {
    if (!started.current && models.ready.length > 0) {
      started.current = true;
      start.mutate();
    }
  }, [start, models.ready.length]);

  if (models.loading) return <Skeleton height={80} />;
  if (models.ready.length === 0) return <NoModel wsId={wsId} />;

  const contextPrefix = targetAgentId
    ? `[Refine the existing agent \`${targetAgentId}\` using admin_patch_agent (id: ${targetAgentId}).]`
    : undefined;
  const placeholder = targetAgentId
    ? app.t(`Describe a change to ${targetAgentId}…`, `描述对 ${targetAgentId} 的修改…`)
    : app.t("Describe the agent you want…", "描述你想要的 agent…");

  return (
    <Gate
      state={gate}
      endpoint={`/v1/agents/${ASSISTANT_ID}`}
      note={app.t("the in-console Admin Assistant agent is not installed", "控制台助手 agent 尚未安装")}
    >
      {() =>
        !sid ? (
          <Skeleton height={80} />
        ) : (
          <>
            {targetAgentId && (
              <div className="row" style={{ gap: 6, marginBottom: 6 }}>
                <span className="mut" style={{ fontSize: 12 }}>{app.t("Refining", "正在修改")}</span>
                <Pill tone="agent">{targetAgentId}</Pill>
              </div>
            )}
            <Transcript
              base={ws(`/v1/sessions/${sid}`)}
              queryKey={["assistant-events", sid]}
              placeholder={placeholder}
              contextPrefix={contextPrefix}
            />
            <DraftedAgents base={ws(`/v1/sessions/${sid}`)} wsId={wsId} />
          </>
        )
      }
    </Gate>
  );
}

export default function AssistantSurface() {
  const app = useApp();
  const { ws: wsId = "default" } = useParams();
  return (
    <Card>
      <h2>{app.t("Admin Assistant", "控制台助手")}</h2>
      <p className="hint">
        {app.t(
          "Describe an agent in plain English — the assistant reads platform capabilities and drafts it (tools, plugins, state machine, permissions, tool overrides). Drafts land in the editor for you to review and publish.",
          "用自然语言描述一个 agent——助手读取平台能力并起草它(工具、插件、状态机、权限、工具覆盖)。草稿会进入编辑器供你审阅并发布。",
        )}
      </p>
      <AssistantPanel wsId={wsId} />
    </Card>
  );
}
