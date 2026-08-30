// One runtime behavior as a named card (agent-editor Behavior section): a title +
// description + enable toggle, and — when enabled — its config as a friendly
// schema-driven form (reusing `SchemaForm`) with an "Advanced (raw JSON)" escape hatch
// so no field is ever unreachable. A plugin with no schema falls back to the JSON box
// directly. Enabling adds the plugin id to the agent's `plugins`; the form writes its
// `plugin_config[id]` section. Friendly names/descriptions come from BEHAVIORS; an
// unknown plugin falls back to its id (its config still renders).

import { useState } from "react";
import { Card, Pill, SchemaForm, Switch } from "../ui";
import type { JsonSchema } from "../ui";
import { useApp } from "../../lib/app-state";
import StateMachineEditor from "./StateMachineEditor";
import WebSearchBehaviorEditor, { defaultWebSearchConfig } from "./WebSearchBehaviorEditor";
import type { CredentialSource } from "../../lib/api/types";

/** Friendly title + one-line description for a known runtime plugin. */
export const BEHAVIORS: Record<string, { title: string; zh: string; desc: string; descZh: string }> = {
  compact: {
    title: "Auto-compaction",
    zh: "自动压缩",
    desc: "When the conversation gets long, summarize the older turns in the background so the model keeps the essentials without the full history.",
    descZh: "对话变长时，在后台把较早的消息总结压缩，让模型保留要点而不必携带全部历史。",
  },
  memory: {
    title: "Memory extraction & recall tuning",
    zh: "记忆提取与召回调优",
    desc: "Configure this Agent's background extraction prompts and optional recall bounds. Whether recall runs is governed by each bound Memory Store policy.",
    descZh: "配置该 Agent 的后台提取提示词和可选召回边界；是否执行召回由各个已绑定记忆库的策略决定。",
  },
  state_machine: {
    title: "Agent behavior state machine",
    zh: "Agent 行为状态机",
    desc: "Control tool preconditions, durable run/thread state, lifecycle facts, reminders, and completion constraints as agent configuration.",
    descZh: "在 Agent 配置中统一控制工具前置条件、run/thread 持久状态、生命周期事实、reminder 与完成约束。",
  },
  web_search: {
    title: "Web search",
    zh: "网页搜索",
    desc: "Choose a free or paid search provider and bind paid API access to an exact Vault credential revision.",
    descZh: "选择免费或付费搜索供应商，并把付费 API 绑定到 Vault 中的精确凭证版本。",
  },
  web_fetch: {
    title: "Web fetch",
    zh: "网页抓取",
    desc: "Choose Awaken-hosted multi-provider routing or the exact model provider's server fetch tool.",
    descZh: "选择由 Awaken 执行的多供应商路由，或精确模型供应商提供的 Server Fetch 工具。",
  },
  background_task: {
    title: "Background tool execution",
    zh: "后台工具执行",
    desc: "Allow selected existing tools to continue after the current Run step; their original permission, concurrency, recovery, and resource policies still apply.",
    descZh: "允许指定的已有工具在当前 Run 步骤结束后继续执行；工具原有的权限、并发、恢复与资源策略仍然生效。",
  },
};

export default function BehaviorCard({
  id,
  schema,
  enabled,
  config,
  onToggle,
  onConfig,
  changed = false,
  credentials = [],
  availability,
}: {
  id: string;
  schema?: JsonSchema;
  enabled: boolean;
  config: Record<string, unknown>;
  onToggle: (on: boolean) => void;
  onConfig: (next: unknown) => void;
  /** The assistant changed this behavior since the operator last authored it. */
  changed?: boolean;
  credentials?: CredentialSource[];
  availability?: { supported: boolean; detail: string };
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
    <Card className={`behavior-card${changed ? " agent-change-highlight" : ""}`}>
      <label className="behavior-head">
        <span>
          <div className="behavior-title">
            {title}
            {changed && <span className="agent-change-label">✦ {app.t("Agent updated", "Agent 已更新")}</span>}
            {availability && !availability.supported && (
              <Pill tone="neutral">{app.t("Unavailable for this runtime", "当前 Runtime 不可用")}</Pill>
            )}
          </div>
          <div className="behavior-desc">{desc}</div>
        </span>
        <Switch aria-label={title} checked={enabled} disabled={availability?.supported === false && !enabled} onChange={(e) => {
          const next = e.target.checked;
          if (next && (id === "web_search" || id === "web_fetch") && schema && !config.provider_id) {
            onConfig(defaultWebSearchConfig(schema));
          }
          onToggle(next);
        }} />
      </label>
      {availability && !availability.supported && (
        <div className={enabled ? "banner warn" : "banner info"} style={{ marginTop: 10 }}>
          <span>!</span>
          <span>{availability.detail}{enabled && app.t(
            " Turn it off before publishing.",
            " 请在发布前关闭该能力。",
          )}</span>
        </div>
      )}
      {enabled && (
        <div style={{ marginTop: 10 }}>
          {id === "state_machine" ? (
            // The state machine gets a purpose-built diagram + table editor (not the
            // generic schema form) — its nested graph is far clearer visually.
            <StateMachineEditor value={config} onChange={onConfig} />
          ) : (id === "web_search" || id === "web_fetch") && schema ? (
            <>
              <WebSearchBehaviorEditor toolId={id} schema={schema} value={config} credentials={credentials} onChange={onConfig} />
              <details style={{ marginTop: 8 }}>
                <summary className="mut" style={{ fontSize: 12, cursor: "pointer" }}>
                  {app.t("Advanced (raw JSON)", "高级(原始 JSON)")}
                </summary>
                <div style={{ marginTop: 6 }}>{jsonBox}</div>
              </details>
            </>
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
