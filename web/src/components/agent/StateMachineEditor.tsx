// The state-machine editor: a "diagram + table" view. The diagram (StateDiagram) makes an
// ordering rule like "read before write" legible at a glance; the table below edits every
// part of the machine (states, transitions, `on` trigger, `when` result-condition, `emit`
// reminder, `on_violation`) — and machine-level scope/key + the continuation nudge. Presets
// drop in the three canonical shapes. A raw-JSON escape hatch keeps nothing unreachable.
// It edits the `state_machine` plugin_config value (SmConfig) directly.

import { useState } from "react";
import { Button, Card } from "../ui";
import { useApp } from "../../lib/app-state";
import StateDiagram from "./StateDiagram";
import {
  PRESETS,
  type SmConfig,
  type SmMachine,
  type SmTransition,
  fromList,
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

  const setMachine = (i: number, m: SmMachine | null) => {
    const machines = cfg.machines.slice();
    if (m) machines[i] = m;
    else machines.splice(i, 1);
    onChange({ ...cfg, machines });
  };
  const addPreset = (m: SmMachine) => onChange({ ...cfg, machines: [...cfg.machines, structuredClone(m)] });

  if (raw) {
    return (
      <div style={{ display: "flex", flexDirection: "column", gap: 6 }}>
        <textarea
          className="input mono"
          style={{ minHeight: 180, fontSize: 12 }}
          defaultValue={JSON.stringify(cfg, null, 2)}
          onBlur={(e) => {
            try {
              onChange(asConfig(JSON.parse(e.target.value)));
            } catch {
              /* keep editing on invalid JSON */
            }
          }}
        />
        <Button variant="ghost" style={{ alignSelf: "flex-start" }} onClick={() => setRaw(false)}>
          ← {app.t("Back to visual editor", "回到可视化编辑")}
        </Button>
      </div>
    );
  }

  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 12 }}>
      <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
        <span className="mut" style={{ fontSize: 12 }}>{app.t("Start from a preset:", "从预设开始:")}</span>
        {PRESETS.map((p) => (
          <Button key={p.key} variant="ghost" style={{ height: 26 }} title={app.t(p.hint, p.hintZh)} onClick={() => addPreset(p.machine)}>
            + {app.t(p.label, p.labelZh)}
          </Button>
        ))}
        <Button variant="ghost" style={{ height: 26, marginLeft: "auto" }} onClick={() => setRaw(true)}>
          {app.t("Raw JSON", "原始 JSON")}
        </Button>
      </div>

      {cfg.machines.length === 0 && (
        <span className="mut" style={{ fontSize: 12 }}>
          {app.t("No machine yet — add a preset above, or the assistant can draft one for you.", "还没有状态机——上面加个预设,或让助手起草。")}
        </span>
      )}

      {cfg.machines.map((m, i) => (
        <MachineEditor key={i} machine={m} onChange={(next) => setMachine(i, next)} onRemove={() => setMachine(i, null)} />
      ))}

      {/* Continuation nudge (forces the loop to keep going until machines reach terminal). */}
      <Card style={{ padding: "10px 12px" }}>
        <label style={{ fontSize: 12, color: "var(--fg3)" }}>{app.t("Continuation nudge", "续跑提醒")}</label>
        <div className="row" style={{ gap: 8, marginTop: 6 }}>
          <input
            {...inp}
            type="number"
            min={0}
            style={{ ...inp.style, width: 90 }}
            placeholder="0"
            title={app.t("Max forced continuations while a machine is non-terminal (0 = off)", "机器未到终态时的最大强制续跑次数(0=关)")}
            value={cfg.continuation?.max_continuations ?? ""}
            onChange={(e) => onChange({ ...cfg, continuation: { ...cfg.continuation, max_continuations: Number(e.target.value) || 0 } })}
          />
          <input
            {...inp}
            style={{ ...inp.style, flex: 1 }}
            placeholder={app.t("Nudge message; {summary} = incomplete instances", "提醒文案;{summary}=未完成实例")}
            value={cfg.continuation?.message ?? ""}
            onChange={(e) => onChange({ ...cfg, continuation: { ...cfg.continuation, message: e.target.value || undefined } })}
          />
        </div>
      </Card>
    </div>
  );
}

function MachineEditor({
  machine,
  onChange,
  onRemove,
}: {
  machine: SmMachine;
  onChange: (m: SmMachine) => void;
  onRemove: () => void;
}) {
  const app = useApp();
  const setT = (i: number, t: SmTransition | null) => {
    const transitions = machine.transitions.slice();
    if (t) transitions[i] = t;
    else transitions.splice(i, 1);
    onChange({ ...machine, transitions });
  };
  const addT = () => onChange({ ...machine, transitions: [...machine.transitions, { on: "*", from: machine.initial, to: machine.initial }] });

  return (
    <Card style={{ padding: "12px 14px", display: "flex", flexDirection: "column", gap: 10 }}>
      <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
        <input {...inp} style={{ ...inp.style, width: 150 }} placeholder={app.t("machine name", "机器名")} value={machine.name} onChange={(e) => onChange({ ...machine, name: e.target.value })} />
        <label className="mut" style={{ fontSize: 11 }}>{app.t("initial", "初始态")}</label>
        <input {...inp} style={{ ...inp.style, width: 90 }} value={machine.initial} onChange={(e) => onChange({ ...machine, initial: e.target.value })} />
        <label className="mut" style={{ fontSize: 11 }}>{app.t("terminal", "终态")}</label>
        <input {...inp} style={{ ...inp.style, width: 120 }} placeholder="a, b" value={(machine.terminal ?? []).join(", ")} onChange={(e) => onChange({ ...machine, terminal: splitList(e.target.value) })} />
        <select className="input mono" style={{ height: 30 }} value={machine.scope ?? "thread"} onChange={(e) => onChange({ ...machine, scope: e.target.value as "thread" | "run" })}>
          <option value="thread">thread</option>
          <option value="run">run</option>
        </select>
        <input {...inp} style={{ ...inp.style, width: 120 }} placeholder={app.t("key e.g. {file_path}", "key 如 {file_path}")} value={machine.key ?? ""} onChange={(e) => onChange({ ...machine, key: e.target.value || undefined })} />
        <Button variant="ghost" style={{ height: 26, marginLeft: "auto" }} onClick={onRemove}>✕</Button>
      </div>

      {/* A — the diagram. */}
      <div style={{ background: "var(--surface)", borderRadius: 8, boxShadow: "inset 0 0 0 1px var(--line)", padding: 6 }}>
        <StateDiagram machine={machine} />
      </div>

      {/* B — the transition table. */}
      <div style={{ display: "flex", flexDirection: "column", gap: 8 }}>
        {machine.transitions.map((t, i) => (
          <TransitionRow key={i} t={t} onChange={(next) => setT(i, next)} onRemove={() => setT(i, null)} />
        ))}
        <Button variant="ghost" style={{ alignSelf: "flex-start", height: 26 }} onClick={addT}>+ {app.t("transition", "转移")}</Button>
      </div>
    </Card>
  );
}

function TransitionRow({ t, onChange, onRemove }: { t: SmTransition; onChange: (t: SmTransition) => void; onRemove: () => void }) {
  const app = useApp();
  const emit = t.emit;
  return (
    <div style={{ borderRadius: 8, background: "var(--soft)", padding: "8px 10px", display: "flex", flexDirection: "column", gap: 6 }}>
      <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
        <input {...inp} style={{ ...inp.style, flex: 1, minWidth: 150 }} placeholder={app.t('on — e.g. Write(file_path ~ "*") or *', 'on — 如 Write(file_path ~ "*") 或 *')} value={t.on} onChange={(e) => onChange({ ...t, on: e.target.value })} />
        <input {...inp} style={{ ...inp.style, width: 130 }} placeholder={app.t("from (a, b)", "from (a, b)")} value={fromList(t.from).join(", ")} onChange={(e) => onChange({ ...t, from: splitList(e.target.value) })} />
        <span className="mut">→</span>
        <input {...inp} style={{ ...inp.style, width: 100 }} placeholder="to" value={t.to} onChange={(e) => onChange({ ...t, to: e.target.value })} />
        <Button variant="ghost" style={{ height: 26 }} onClick={onRemove}>✕</Button>
      </div>
      <div className="row" style={{ gap: 6, flexWrap: "wrap", fontSize: 12 }}>
        <select className="input mono" style={{ height: 28 }} value={t.on_violation?.action ?? ""} onChange={(e) => onChange({ ...t, on_violation: e.target.value ? { action: e.target.value, reason: t.on_violation?.reason } : undefined })} title={app.t("on-violation action", "违规动作")}>
          <option value="">{app.t("no gate", "不拦截")}</option>
          <option value="deny">deny</option>
          <option value="warn">warn</option>
        </select>
        {t.on_violation && (
          <input {...inp} style={{ ...inp.style, flex: 1, minWidth: 160, height: 28 }} placeholder={app.t("violation reason (shown to the model)", "违规原因(给模型看)")} value={t.on_violation.reason ?? ""} onChange={(e) => onChange({ ...t, on_violation: { action: t.on_violation!.action, reason: e.target.value || undefined } })} />
        )}
      </div>
      <div className="row" style={{ gap: 6, flexWrap: "wrap", fontSize: 12 }}>
        <span title={app.t("emit a system reminder on this transition", "此转移上注入 system reminder")}>📢</span>
        <input {...inp} style={{ ...inp.style, flex: 1, minWidth: 200, height: 28 }} placeholder={app.t("reminder text (leave empty for none)", "提醒文案(留空=无)")} value={emit?.content ?? ""} onChange={(e) => onChange({ ...t, emit: e.target.value ? { target: emit?.target ?? "suffix_system", content: e.target.value, cooldown_turns: emit?.cooldown_turns } : undefined })} />
        {emit && (
          <input {...inp} type="number" min={0} style={{ ...inp.style, width: 100, height: 28 }} placeholder={app.t("cooldown", "冷却")} title={app.t("cooldown turns between emits", "两次提醒之间的冷却轮数")} value={emit.cooldown_turns ?? ""} onChange={(e) => onChange({ ...t, emit: { ...emit, cooldown_turns: Number(e.target.value) || 0 } })} />
        )}
      </div>
    </div>
  );
}

function splitList(s: string): string[] {
  return s.split(",").map((x) => x.trim()).filter(Boolean);
}
