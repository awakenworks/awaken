// Visual editor for the generic runtime State Machine DSL. The UI follows the
// execution model: identify the instance, choose a trigger/guard, then configure
// the state update and optional effect. Raw JSON remains the lossless escape hatch.

import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import { Button, Card } from "../ui";
import { useApp } from "../../lib/app-state";
import StateDiagram from "./StateDiagram";
import {
  PRESETS,
  type SmConfig,
  type SmMachine,
  type SmTransition,
  fromList,
  triggerKind,
  triggerText,
  withTriggerKind,
} from "./state-machine-presets";

function asConfig(value: unknown): SmConfig {
  const v = (value ?? {}) as SmConfig;
  return { machines: Array.isArray(v.machines) ? v.machines : [], continuation: v.continuation };
}

const inp = { className: "input", style: { height: 30 } } as const;

export default function StateMachineEditor({
  value,
  onChange,
}: {
  value: unknown;
  onChange: (next: SmConfig) => void;
}) {
  const app = useApp();
  const cfg = asConfig(value);
  const [raw, setRaw] = useState(false);

  const setMachine = (index: number, machine: SmMachine | null) => {
    const machines = cfg.machines.slice();
    if (machine) machines[index] = machine;
    else machines.splice(index, 1);
    onChange({ ...cfg, machines });
  };
  const addPreset = (machine: SmMachine) =>
    onChange({ ...cfg, machines: [...cfg.machines, structuredClone(machine)] });

  if (raw) {
    return <RawEditor config={cfg} onChange={onChange} onClose={() => setRaw(false)} />;
  }

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
      <div
        style={{
          padding: "10px 12px",
          borderRadius: 8,
          background: "color-mix(in srgb, var(--agent) 8%, var(--surface))",
          boxShadow: "inset 3px 0 0 var(--agent)",
        }}
      >
        <div style={{ fontWeight: 650, fontSize: 13 }}>
          {app.t("Control agent behavior as data", "用配置控制 Agent 行为")}
        </div>
        <div className="mut" style={{ fontSize: 12, marginTop: 3, lineHeight: 1.5 }}>
          {app.t(
            "Gate a tool before it runs, update durable state from tool results or runtime facts, and inject request-only reminders without polluting the transcript.",
            "在工具执行前拦截；根据工具结果或运行事实更新持久状态；按需注入不污染对话历史的 reminder。",
          )}
        </div>
      </div>

      <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
        <span className="mut" style={{ fontSize: 12 }}>
          {app.t("Choose an intent:", "选择意图:")}
        </span>
        {PRESETS.map((preset) => (
          <Button
            key={preset.key}
            variant="ghost"
            style={{ height: 28 }}
            title={`${app.t(preset.hint, preset.hintZh)} ${app.t(preset.value, preset.valueZh)}`}
            onClick={() => addPreset(preset.machine)}
          >
            + {app.t(preset.label, preset.labelZh)}
          </Button>
        ))}
        <Button variant="ghost" style={{ height: 28, marginLeft: "auto" }} onClick={() => setRaw(true)}>
          {"{}"} {app.t("JSON", "JSON")}
        </Button>
      </div>

      {cfg.machines.length === 0 && (
        <span className="mut" style={{ fontSize: 12 }}>
          {app.t(
            "No machine yet. Start with an intent above; every generated field remains editable.",
            "还没有状态机。先选择上面的意图，生成后的每个字段都可继续编辑。",
          )}
        </span>
      )}

      {cfg.machines.map((machine, index) => (
        <MachineEditor
          key={`${machine.name}-${index}`}
          machine={machine}
          onChange={(next) => setMachine(index, next)}
          onRemove={() => setMachine(index, null)}
        />
      ))}

      <ContinuationEditor config={cfg} onChange={onChange} />
    </div>
  );
}

function RawEditor({
  config,
  onChange,
  onClose,
}: {
  config: SmConfig;
  onChange: (next: SmConfig) => void;
  onClose: () => void;
}) {
  const app = useApp();
  const [draft, setDraft] = useState(() => JSON.stringify(config, null, 2));
  const [error, setError] = useState("");
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
      <textarea
        className="input mono"
        style={{ minHeight: 260, fontSize: 12 }}
        value={draft}
        onChange={(event) => setDraft(event.target.value)}
        onBlur={() => {
          try {
            onChange(asConfig(JSON.parse(draft)));
            setError("");
          } catch {
            setError(app.t("Invalid JSON — changes were not applied.", "JSON 非法，修改未应用。"));
          }
        }}
      />
      {error && <span className="err" style={{ fontSize: 11 }}>{error}</span>}
      <Button variant="ghost" style={{ alignSelf: "flex-start" }} onClick={onClose}>
        ← {app.t("Visual editor", "可视化编辑")}
      </Button>
    </div>
  );
}

function MachineEditor({
  machine,
  onChange,
  onRemove,
}: {
  machine: SmMachine;
  onChange: (machine: SmMachine) => void;
  onRemove: () => void;
}) {
  const app = useApp();
  const setTransition = (index: number, transition: SmTransition | null) => {
    const transitions = machine.transitions.slice();
    if (transition) transitions[index] = transition;
    else transitions.splice(index, 1);
    onChange({ ...machine, transitions });
  };
  const addTransition = () =>
    onChange({
      ...machine,
      transitions: [
        ...machine.transitions,
        { on: "*", from: machine.initial, to: machine.initial },
      ],
    });

  return (
    <Card style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 10 }}>
      <div className="row" style={{ justifyContent: "space-between", gap: 8 }}>
        <div>
          <div style={{ fontWeight: 650, fontSize: 13 }}>{app.t("1 · Instance & lifetime", "1 · 实例与生命周期")}</div>
          <div className="mut" style={{ fontSize: 11, marginTop: 2 }}>
            {machine.scope === "run"
              ? app.t("Reset when a new run starts", "新 run 启动时重置")
              : app.t("Persist across runs in this thread", "在同一 thread 的多个 run 间持久化")}
          </div>
        </div>
        <Button variant="ghost" style={{ height: 26 }} onClick={onRemove}>✕</Button>
      </div>

      <div className="row" style={{ gap: 7, flexWrap: "wrap" }}>
        <Labeled label={app.t("Machine", "机器")}>
          <input {...inp} style={{ ...inp.style, width: 160 }} value={machine.name} onChange={(e) => onChange({ ...machine, name: e.target.value })} />
        </Labeled>
        <Labeled label={app.t("Scope", "作用域")}>
          <select className="input mono" style={{ height: 30 }} value={machine.scope ?? "thread"} onChange={(e) => onChange({ ...machine, scope: e.target.value as "thread" | "run" })}>
            <option value="thread">thread</option>
            <option value="run">run</option>
          </select>
        </Labeled>
        <Labeled label={app.t("Instance key", "实例 key")}>
          <input {...inp} style={{ ...inp.style, width: 135 }} placeholder="{path}" value={machine.key ?? ""} onChange={(e) => onChange({ ...machine, key: e.target.value || undefined })} />
        </Labeled>
        <Labeled label={app.t("Normalize", "标准化")}>
          <select className="input mono" style={{ height: 30 }} value={machine.key_normalizer ?? "none"} onChange={(e) => onChange({ ...machine, key_normalizer: e.target.value as SmMachine["key_normalizer"] })}>
            {['none', 'trim', 'lowercase', 'path', 'url'].map((value) => <option key={value}>{value}</option>)}
          </select>
        </Labeled>
        <Labeled label={app.t("Initial", "初始态")}>
          <input {...inp} style={{ ...inp.style, width: 90 }} value={machine.initial} onChange={(e) => onChange({ ...machine, initial: e.target.value })} />
        </Labeled>
        <Labeled label={app.t("Terminal", "终态")}>
          <input {...inp} style={{ ...inp.style, width: 120 }} placeholder="done, idle" value={(machine.terminal ?? []).join(", ")} onChange={(e) => onChange({ ...machine, terminal: splitList(e.target.value) })} />
        </Labeled>
        <label className="row mut" style={{ gap: 5, fontSize: 11, alignSelf: "flex-end", height: 30 }}>
          <input type="checkbox" checked={machine.strict ?? false} onChange={(e) => onChange({ ...machine, strict: e.target.checked || undefined })} />
          {app.t("deny unmatched tools", "拒绝未声明工具")}
        </label>
      </div>

      <div style={{ background: "var(--surface)", borderRadius: 8, boxShadow: "inset 0 0 0 1px var(--line)", padding: 6 }}>
        <StateDiagram machine={machine} />
      </div>

      <div>
        <div style={{ fontWeight: 650, fontSize: 13 }}>{app.t("2 · Trigger, guard & effect", "2 · 触发、条件与效果")}</div>
        <div className="mut" style={{ fontSize: 11, marginTop: 2 }}>
          {app.t("Tool violations gate before execution. Result transitions run after execution. Events update atomically.", "工具违规在执行前拦截；结果转移在执行后发生；事件更新保持原子性。")}
        </div>
      </div>
      <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        {machine.transitions.map((transition, index) => (
          <TransitionRow
            key={index}
            transition={transition}
            onChange={(next) => setTransition(index, next)}
            onRemove={() => setTransition(index, null)}
          />
        ))}
        <Button variant="ghost" style={{ alignSelf: "flex-start", height: 28 }} onClick={addTransition}>
          + {app.t("Transition", "转移")}
        </Button>
      </div>
    </Card>
  );
}

function TransitionRow({
  transition,
  onChange,
  onRemove,
}: {
  transition: SmTransition;
  onChange: (transition: SmTransition) => void;
  onRemove: () => void;
}) {
  const app = useApp();
  const kind = triggerKind(transition.on);
  const emit = transition.emit;
  const whenStatus = typeof transition.when === "string" ? transition.when : transition.when?.status;
  const whenContent = typeof transition.when === "object" ? transition.when.content : undefined;

  return (
    <div style={{ borderRadius: 8, background: "var(--soft)", padding: "9px 10px", display: "flex", flexDirection: "column", gap: 7 }}>
      <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
          <select aria-label={app.t("Trigger type", "触发类型")} className="input mono" style={{ height: 30, width: 92 }} value={kind} onChange={(e) => onChange(withTriggerKind(transition, e.target.value as "tool" | "event"))}>
          <option value="tool">tool</option>
          <option value="event">event</option>
        </select>
        <input
          {...inp}
          className="input mono"
          style={{ ...inp.style, flex: 1, minWidth: 190 }}
          placeholder={kind === "tool" ? 'write(path ~ "*")' : "step.before_inference"}
          value={triggerText(transition.on)}
          onChange={(e) => onChange({ ...transition, on: kind === "tool" ? e.target.value : { event: e.target.value } })}
        />
        <input {...inp} className="input mono" style={{ ...inp.style, width: 130 }} placeholder="from: a, b" value={fromList(transition.from).join(", ")} onChange={(e) => onChange({ ...transition, from: splitList(e.target.value) })} />
        <span className="mut">→</span>
        <input {...inp} className="input mono" style={{ ...inp.style, width: 100 }} placeholder="to" value={transition.to} onChange={(e) => onChange({ ...transition, to: e.target.value })} />
        <span className="pill" style={{ fontSize: 10 }}>
          {kind === "tool" ? app.t("pre + post", "执行前 + 后") : app.t("atomic fact", "原子事实")}
        </span>
        <Button variant="ghost" style={{ height: 26 }} onClick={onRemove}>✕</Button>
      </div>

      {kind === "tool" && (
        <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
          <span className="mut" style={{ fontSize: 11, width: 72 }}>{app.t("Before run", "执行前")}</span>
          <select aria-label={app.t("Before run action", "执行前动作")} className="input mono" style={{ height: 28 }} value={transition.on_violation?.action ?? ""} onChange={(e) => onChange({ ...transition, on_violation: e.target.value ? { action: e.target.value as "deny" | "ask" | "warn", reason: transition.on_violation?.reason } : undefined })}>
            <option value="">{app.t("allow", "允许")}</option>
            <option value="deny">deny</option>
            <option value="ask">ask</option>
            <option value="warn">warn</option>
          </select>
          {transition.on_violation && (
            <input {...inp} style={{ ...inp.style, flex: 1, minWidth: 220, height: 28 }} placeholder={app.t("Reason shown to the model", "给模型看的原因") } value={transition.on_violation.reason ?? ""} onChange={(e) => onChange({ ...transition, on_violation: { ...transition.on_violation!, reason: e.target.value || undefined } })} />
          )}
        </div>
      )}

      <details>
        <summary className="mut" style={{ fontSize: 11, cursor: "pointer" }}>
          {app.t("Conditions & durable update", "条件与持久化更新")}
        </summary>
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(210px, 1fr))", gap: 7, marginTop: 7 }}>
          {kind === "tool" && (
            <div className="row" style={{ gap: 6 }}>
              <select className="input mono" style={{ height: 28, flex: 1 }} value={whenStatus ?? ""} onChange={(e) => onChange({ ...transition, when: e.target.value || whenContent ? { status: e.target.value || undefined, content: whenContent } : undefined })}>
                <option value="">when: success (default)</option>
                <option value="success">when: success</option>
                <option value="error">when: error</option>
                <option value="any">when: any</option>
              </select>
              <input {...inp} style={{ ...inp.style, flex: 1, height: 28 }} placeholder="result content glob" value={whenContent ?? ""} onChange={(e) => onChange({ ...transition, when: e.target.value || whenStatus ? { status: whenStatus, content: e.target.value || undefined } : undefined })} />
            </div>
          )}
          <JsonObjectField label={app.t("Counter guards", "计数条件")} value={transition.counters} placeholder='{"steps":{"gte":3}}' onChange={(counters) => onChange({ ...transition, counters })} />
          <JsonObjectField label={app.t("Capture data", "捕获数据")} value={transition.update?.capture} placeholder='{"summary":"{event.data.summary}"}' onChange={(capture) => onChange({ ...transition, update: compactUpdate({ ...transition.update, capture }) })} />
          <input {...inp} style={{ ...inp.style, height: 28 }} placeholder={app.t("increment: steps, retries", "递增: steps, retries")} value={(transition.update?.increment ?? []).join(", ")} onChange={(e) => onChange({ ...transition, update: compactUpdate({ ...transition.update, increment: splitList(e.target.value) }) })} />
          <input {...inp} style={{ ...inp.style, height: 28 }} placeholder={app.t("reset: steps", "重置: steps")} value={(transition.update?.reset ?? []).join(", ")} onChange={(e) => onChange({ ...transition, update: compactUpdate({ ...transition.update, reset: splitList(e.target.value) }) })} />
        </div>
      </details>

      <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
        <span className="mut" style={{ fontSize: 11, width: 72 }}>{app.t("Reminder", "提醒")}</span>
        <input {...inp} style={{ ...inp.style, flex: 1, minWidth: 240, height: 28 }} placeholder={app.t("Optional message to the model", "可选：给模型的消息")} value={emit?.content ?? ""} onChange={(e) => onChange({ ...transition, emit: e.target.value ? { target: kind === "event" ? "context" : emit?.target ?? "suffix_system", content: e.target.value, cooldown_steps: emit?.cooldown_steps } : undefined })} />
        {emit && (
          <>
            <select aria-label={app.t("Reminder target", "提醒目标")} className="input mono" style={{ height: 28 }} value={kind === "event" ? "context" : emit.target ?? "suffix_system"} disabled={kind === "event"} onChange={(e) => onChange({ ...transition, emit: { ...emit, target: e.target.value as NonNullable<typeof emit.target> } })}>
              <option value="context">context</option>
              <option value="system">system</option>
              <option value="suffix_system">suffix_system</option>
              <option value="session">session</option>
              <option value="conversation">conversation</option>
            </select>
            <input {...inp} type="number" min={0} style={{ ...inp.style, width: 118, height: 28 }} placeholder="cooldown steps" title={app.t("Completed inference steps between reminders", "两次提醒之间完成的 inference step 数")} value={emit.cooldown_steps ?? ""} onChange={(e) => onChange({ ...transition, emit: { ...emit, cooldown_steps: Number(e.target.value) || 0 } })} />
          </>
        )}
      </div>
    </div>
  );
}

function ContinuationEditor({ config, onChange }: { config: SmConfig; onChange: (next: SmConfig) => void }) {
  const app = useApp();
  return (
    <Card style={{ padding: "10px 12px" }}>
      <div style={{ fontWeight: 650, fontSize: 13 }}>{app.t("3 · Completion constraint", "3 · 完成约束")}</div>
      <div className="mut" style={{ fontSize: 11, marginTop: 2 }}>
        {app.t("Optionally continue while a materialized instance has not reached a terminal state.", "可选：当已实例化的状态尚未到达终态时继续运行。")}
      </div>
      <div className="row" style={{ gap: 8, marginTop: 7 }}>
        <input {...inp} type="number" min={0} style={{ ...inp.style, width: 100 }} placeholder="max: 0" value={config.continuation?.max_continuations ?? ""} onChange={(e) => onChange({ ...config, continuation: { ...config.continuation, max_continuations: Number(e.target.value) || 0 } })} />
        <input {...inp} style={{ ...inp.style, flex: 1 }} placeholder={app.t("Continue message; {summary} = incomplete instances", "继续文案；{summary}=未完成实例")} value={config.continuation?.message ?? ""} onChange={(e) => onChange({ ...config, continuation: { ...config.continuation, message: e.target.value || undefined } })} />
      </div>
    </Card>
  );
}

function JsonObjectField<T extends Record<string, unknown>>({
  label,
  value,
  placeholder,
  onChange,
}: {
  label: string;
  value?: T;
  placeholder: string;
  onChange: (value: T | undefined) => void;
}) {
  const [draft, setDraft] = useState(value && Object.keys(value).length ? JSON.stringify(value) : "");
  const [error, setError] = useState(false);
  useEffect(() => {
    setDraft(value && Object.keys(value).length ? JSON.stringify(value) : "");
  }, [value]);
  return (
    <input
      {...inp}
      className={`input mono${error ? " err" : ""}`}
      style={{ ...inp.style, height: 28 }}
      aria-label={label}
      title={label}
      placeholder={`${label}: ${placeholder}`}
      value={draft}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => {
        try {
          const parsed = draft.trim() ? JSON.parse(draft) : undefined;
          if (parsed !== undefined && (Array.isArray(parsed) || typeof parsed !== "object" || parsed === null)) throw new Error();
          onChange(parsed as T | undefined);
          setError(false);
        } catch {
          setError(true);
        }
      }}
    />
  );
}

function Labeled({ label, children }: { label: string; children: ReactNode }) {
  return <label style={{ display: "flex", flexDirection: "column", gap: 3, fontSize: 10, color: "var(--fg3)" }}>{label}{children}</label>;
}

function compactUpdate(update: SmTransition["update"]): SmTransition["update"] {
  if (!update) return undefined;
  const capture = update.capture && Object.keys(update.capture).length ? update.capture : undefined;
  const increment = update.increment?.length ? update.increment : undefined;
  const reset = update.reset?.length ? update.reset : undefined;
  return capture || increment || reset ? { capture, increment, reset } : undefined;
}

function splitList(value: string): string[] {
  return value.split(",").map((part) => part.trim()).filter(Boolean);
}
