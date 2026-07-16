// One runtime behavior as a named card (agent-editor Behavior section): a title +
// description + enable toggle, and — when enabled — its config as a friendly
// schema-driven form (reusing `SchemaForm`) with an "Advanced (raw JSON)" escape hatch
// so no field is ever unreachable. A plugin with no schema falls back to the JSON box
// directly. Enabling adds the plugin id to the agent's `plugins`; the form writes its
// `plugin_config[id]` section. Friendly names/descriptions come from BEHAVIORS; an
// unknown plugin falls back to its id (its config still renders).

import { useState } from "react";
import { Card, SchemaForm, Switch } from "../ui";
import type { JsonSchema } from "../ui";
import { useApp } from "../../lib/app-state";
import StateMachineEditor from "./StateMachineEditor";

/** Friendly title + one-line description for a known runtime plugin. */
export const BEHAVIORS: Record<string, { title: string; zh: string; desc: string; descZh: string }> = {
  compact: {
    title: "Auto-compaction",
    zh: "自动压缩",
    desc: "When the conversation gets long, summarize the older turns in the background so the model keeps the essentials without the full history.",
    descZh: "对话变长时,在后台把较早的轮次总结压缩,让模型保留要点而不必带上全部历史。",
  },
  memory: {
    title: "Memory recall",
    zh: "记忆召回",
    desc: "Recall relevant long-term memories into context on demand.",
    descZh: "按需把相关的长期记忆召回到上下文。",
  },
  state_machine: {
    title: "Tool-call ordering & reminders",
    zh: "工具顺序与提醒",
    desc: "A state machine over tool calls: constrain their order (e.g. read a file before writing it) and/or emit periodic system reminders (background tasks, todos).",
    descZh: "一台跑在工具调用上的状态机:约束调用顺序(如先读后写),并/或周期性注入 system reminder(后台任务、待办)。",
  },
};

export default function BehaviorCard({
  id,
  schema,
  enabled,
  config,
  onToggle,
  onConfig,
}: {
  id: string;
  schema?: JsonSchema;
  enabled: boolean;
  config: Record<string, unknown>;
  onToggle: (on: boolean) => void;
  onConfig: (next: unknown) => void;
}) {
  const app = useApp();
  const meta = BEHAVIORS[id];
  const title = meta ? app.t(meta.title, meta.zh) : id;
  const desc = meta ? app.t(meta.desc, meta.descZh) : app.t("Runtime plugin.", "运行时插件。");
  const [draft, setDraft] = useState<string | null>(null);
  const [err, setErr] = useState("");
  const commitJson = (raw: string) => {
    setDraft(raw);
    try {
      onConfig(raw.trim() === "" ? {} : JSON.parse(raw));
      setErr("");
    } catch {
      setErr(app.t("invalid JSON — not saved", "JSON 非法 — 未保存"));
    }
  };
  const jsonBox = (
    <>
      <textarea
        className="input mono"
        rows={5}
        value={draft ?? JSON.stringify(config ?? {}, null, 2)}
        onChange={(e) => commitJson(e.target.value)}
      />
      {err && <span className="err" style={{ fontSize: 11 }}>{err}</span>}
    </>
  );
  return (
    <Card className="behavior-card">
      <label className="behavior-head">
        <span>
          <div className="behavior-title">{title}</div>
          <div className="behavior-desc">{desc}</div>
        </span>
        <Switch aria-label={title} checked={enabled} onChange={(e) => onToggle(e.target.checked)} />
      </label>
      {enabled && (
        <div style={{ marginTop: 10 }}>
          {id === "state_machine" ? (
            // The state machine gets a purpose-built diagram + table editor (not the
            // generic schema form) — its nested graph is far clearer visually.
            <StateMachineEditor value={config} onChange={onConfig} />
          ) : schema ? (
            <>
              <SchemaForm schema={schema} value={config} onChange={onConfig} />
              <details style={{ marginTop: 8 }}>
                <summary className="mut" style={{ fontSize: 12, cursor: "pointer" }}>
                  {app.t("Advanced (raw JSON)", "高级(原始 JSON)")}
                </summary>
                <div style={{ marginTop: 6 }}>{jsonBox}</div>
              </details>
            </>
          ) : (
            jsonBox
          )}
        </div>
      )}
    </Card>
  );
}
