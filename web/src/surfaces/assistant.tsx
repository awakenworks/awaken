// Admin Assistant: an in-console copilot that authors agents in plain English. It is a
// chat against the built-in `__admin_assistant` agent, so it reuses the same <Transcript>
// engine as session detail and the editor Sandbox. Its authoring tools PERSIST each draft
// as an unpublished config agent, so the loop closes here: describe → the assistant drafts
// → you Open it in the editor → review the diff → Publish. Truth-driven: gates when the
// assistant agent is not installed, or when no model is configured to run it on.

import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router";
import Gate from "../components/app/Gate";
import Transcript from "../components/session/Transcript";
import { Button, Card, Pill, Skeleton } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { Session } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useGate } from "../lib/useGate";
import { useModels } from "../lib/useModels";

const ASSISTANT_ID = "__admin_assistant";
// The authoring tools whose tool-use input names the agent they draft/patch.
const DRAFT_TOOLS = ["admin_draft_agent", "admin_patch_agent"];

interface EventLike {
  type: string;
  name?: string;
  input?: { id?: string };
}

/** The agents this assistant session has drafted so far — read from its own tool calls
 * (each `admin_draft_agent`/`admin_patch_agent` names the agent in its input). Each is a
 * real unpublished config agent now, so we can deep-link the operator into the editor. */
function DraftedAgents({ base, wsId }: { base: string; wsId: string }) {
  const app = useApp();
  const nav = useNavigate();
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
    <Card style={{ marginTop: 12 }}>
      <div className="row" style={{ gap: 8, flexWrap: "wrap", alignItems: "center" }}>
        <span className="mut" style={{ fontSize: 12 }}>
          {app.t("Drafted this session — review & publish:", "本次起草——去审阅并发布:")}
        </span>
        {ids.map((id) => (
          <Button key={id} variant="ghost" style={{ height: 26 }} onClick={() => nav(`/w/${wsId}/agents/${id}`)}>
            <Pill tone="agent">{id}</Pill>&nbsp;{app.t("Open in editor →", "在编辑器打开 →")}
          </Button>
        ))}
      </div>
    </Card>
  );
}

function AssistantChat({ wsId }: { wsId: string }) {
  const app = useApp();
  const [sid, setSid] = useState<string | null>(null);
  const started = useRef(false);
  const start = useMutation({
    mutationFn: () =>
      api.post<Session>(ws("/v1/sessions"), { agent: ASSISTANT_ID, title: "admin assistant" }),
    onSuccess: (s) => setSid(s.id),
  });
  // Open one scratch session per visit.
  useEffect(() => {
    if (!started.current) {
      started.current = true;
      start.mutate();
    }
  }, [start]);

  if (!sid) return <Skeleton height={80} />;
  const base = ws(`/v1/sessions/${sid}`);
  return (
    <>
      <Transcript
        base={base}
        queryKey={["assistant-events", sid]}
        placeholder={app.t("Describe the agent you want…", "描述你想要的 agent…")}
      />
      <DraftedAgents base={base} wsId={wsId} />
    </>
  );
}

/** Shown when the platform has no credentialed model — the assistant can't run without one,
 * so we say so plainly and point to Models, rather than opening a chat that errors. */
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

export default function AssistantSurface() {
  const app = useApp();
  const { ws: wsId = "default" } = useParams();
  const gate = useGate(`/v1/agents/${ASSISTANT_ID}`);
  const models = useModels();
  return (
    <Card>
      <h2>{app.t("Admin Assistant", "控制台助手")}</h2>
      <p className="hint">
        {app.t(
          "Describe an agent in plain English — the assistant reads platform capabilities and drafts it (tools, plugins, state machine, permissions, tool overrides). Drafts land in the editor for you to review and publish.",
          "用自然语言描述一个 agent——助手读取平台能力并起草它(工具、插件、状态机、权限、工具覆盖)。草稿会进入编辑器供你审阅并发布。",
        )}
      </p>
      <Gate
        state={gate}
        endpoint={`/v1/agents/${ASSISTANT_ID}`}
        note={app.t("the in-console Admin Assistant agent is not installed", "控制台助手 agent 尚未安装")}
      >
        {() =>
          models.loading ? (
            <Skeleton height={80} />
          ) : models.ready.length === 0 ? (
            <NoModel wsId={wsId} />
          ) : (
            <AssistantChat wsId={wsId} />
          )
        }
      </Gate>
    </Card>
  );
}
