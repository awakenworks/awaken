// Admin Assistant: an in-console copilot that drafts agents in plain English. It
// is just a chat against the built-in `__admin_assistant` agent, so it reuses the
// same <Transcript> engine as the session detail and the editor Sandbox — the
// assistant's drafts arrive as tool calls in the transcript. Truth-driven: if the
// assistant agent is not installed, the surface gates instead of faking a copilot.

import { useMutation } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useParams } from "react-router";
import Gate from "../components/app/Gate";
import Transcript from "../components/session/Transcript";
import { Card, Skeleton } from "../components/ui";
import { api, ws } from "../lib/api/client";
import type { Session } from "../lib/api/types";
import { useApp } from "../lib/app-state";
import { useGate } from "../lib/useGate";

const ASSISTANT_ID = "__admin_assistant";

function AssistantChat() {
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
  return (
    <Transcript
      base={ws(`/v1/sessions/${sid}`)}
      queryKey={["assistant-events", sid]}
      placeholder={app.t("Describe the agent you want…", "描述你想要的 agent…")}
    />
  );
}

export default function AssistantSurface() {
  const app = useApp();
  useParams(); // keep the surface workspace-aware (ws() reads the active scope)
  const gate = useGate(`/v1/agents/${ASSISTANT_ID}`);
  return (
    <Card>
      <h2>{app.t("Admin Assistant", "控制台助手")}</h2>
      <p className="hint">
        {app.t(
          "Describe an agent in plain English — the assistant reads platform capabilities and drafts it for you.",
          "用自然语言描述一个 agent——助手会读取平台能力并为你起草。",
        )}
      </p>
      <Gate
        state={gate}
        endpoint={`/v1/agents/${ASSISTANT_ID}`}
        note={app.t(
          "the in-console Admin Assistant agent is not installed",
          "控制台助手 agent 尚未安装",
        )}
      >
        {() => <AssistantChat />}
      </Gate>
    </Card>
  );
}
