// Valid starter machines for the runtime State Machine DSL. The domain stays
// generic: TODO/background semantics enter as adapter facts, never hard-coded
// operations in the machine engine.

export type SmTrigger = string | { event: string };

export interface SmEmit {
  target?: "context" | "system" | "suffix_system" | "session" | "conversation";
  content: string;
  cooldown_steps?: number;
  role?: "user" | "assistant";
}

export interface SmViolation {
  action: "deny" | "ask" | "warn";
  reason?: string;
}

export interface SmCounterCondition {
  gte?: number;
  lte?: number;
}

export interface SmUpdate {
  capture?: Record<string, string>;
  increment?: string[];
  reset?: string[];
}

export interface SmTransition {
  on: SmTrigger;
  from: string | string[];
  to: string;
  when?: string | { status?: string; content?: string };
  counters?: Record<string, SmCounterCondition>;
  update?: SmUpdate;
  emit?: SmEmit;
  on_violation?: SmViolation;
}

export interface SmMachine {
  name: string;
  scope?: "thread" | "run";
  key?: string;
  key_normalizer?: "none" | "trim" | "lowercase" | "path" | "url";
  initial: string;
  strict?: boolean;
  on_unmatched?: string;
  terminal?: string[];
  transitions: SmTransition[];
}

export interface SmConfig {
  machines: SmMachine[];
  continuation?: { max_continuations?: number; message?: string };
}

export interface Preset {
  key: string;
  label: string;
  labelZh: string;
  hint: string;
  hintZh: string;
  value: string;
  valueZh: string;
  machine: SmMachine;
}

export const PRESETS: Preset[] = [
  {
    key: "read-before-write",
    label: "Read before write",
    labelZh: "先读后写",
    hint: "Block a write before execution until the same path has been read.",
    hintZh: "写工具执行前拦截；同一路径成功读取后才允许写。",
    value: "Prevent destructive guesses while allowing repeated edits after one read.",
    valueZh: "避免盲写；一次读取后仍可连续编辑。",
    machine: {
      name: "read-before-write",
      scope: "thread",
      key: "{path}",
      key_normalizer: "path",
      initial: "unread",
      terminal: ["written"],
      transitions: [
        { on: 'read(path ~ "*")', from: ["unread", "read", "written"], to: "read" },
        {
          on: 'write(path ~ "*")',
          from: ["read", "written"],
          to: "written",
          when: { status: "success" },
          on_violation: { action: "deny", reason: "Read {path} before writing it." },
        },
      ],
    },
  },
  {
    key: "background-task-reminder",
    label: "Background-task reminder",
    labelZh: "后台任务提醒",
    hint: "Use generic background.started/completed facts to remind only while work is active.",
    hintZh: "消费通用 background.started/completed 事实，仅在后台工作活跃时提醒。",
    value: "Completion delivery stays automatic; the machine only controls state and reminders.",
    valueZh: "完成消息仍自动投递；状态机只控制状态和提醒。",
    machine: {
      name: "background-task",
      scope: "thread",
      key: "{task_id}",
      initial: "idle",
      terminal: ["idle"],
      transitions: [
        {
          on: { event: "background.started" },
          from: ["idle", "active"],
          to: "active",
          update: { capture: { summary: "{event.data.summary}" }, reset: ["steps"] },
        },
        {
          on: { event: "step.after_inference" },
          from: "active",
          to: "active",
          update: { increment: ["steps"] },
        },
        {
          on: { event: "step.before_inference" },
          from: "active",
          to: "active",
          counters: { steps: { gte: 3 } },
          emit: {
            target: "context",
            content: "Background work is still active: {instance.data.summary}",
            cooldown_steps: 3,
          },
          update: { reset: ["steps"] },
        },
        {
          on: { event: "background.completed" },
          from: "active",
          to: "idle",
          update: { capture: { result: "{event.data.summary}" }, reset: ["steps"] },
        },
      ],
    },
  },
  {
    key: "todo-reminder",
    label: "Todo reminder",
    labelZh: "待办提醒",
    hint: "Capture todo.changed facts and inject the latest snapshot as request-only context.",
    hintZh: "捕获 todo.changed 事实，将最新清单作为仅当前请求可见的上下文注入。",
    value: "Keeps the model oriented without polluting the conversation transcript.",
    valueZh: "保持模型方向感，同时不污染对话历史。",
    machine: {
      name: "todo",
      scope: "thread",
      key: "",
      initial: "tracking",
      transitions: [
        {
          on: { event: "todo.changed" },
          from: "tracking",
          to: "tracking",
          update: { capture: { snapshot: "{event.data.snapshot}" }, reset: ["steps"] },
        },
        {
          on: { event: "step.after_inference" },
          from: "tracking",
          to: "tracking",
          update: { increment: ["steps"] },
        },
        {
          on: { event: "step.before_inference" },
          from: "tracking",
          to: "tracking",
          counters: { steps: { gte: 5 } },
          emit: {
            target: "context",
            content: "Review and update the current TODO list: {instance.data.snapshot}",
            cooldown_steps: 5,
          },
          update: { reset: ["steps"] },
        },
      ],
    },
  },
];

export function fromList(from: string | string[]): string[] {
  return Array.isArray(from) ? from : [from];
}

export function triggerKind(on: SmTrigger): "tool" | "event" {
  return typeof on === "string" ? "tool" : "event";
}

export function triggerText(on: SmTrigger): string {
  return typeof on === "string" ? on : on.event;
}

export function withTriggerKind(
  transition: SmTransition,
  kind: "tool" | "event",
): SmTransition {
  const text = triggerText(transition.on) || (kind === "tool" ? "*" : "step.before_inference");
  if (kind === "tool") return { ...transition, on: text };
  return {
    ...transition,
    on: { event: text },
    when: undefined,
    on_violation: undefined,
    emit: transition.emit ? { ...transition.emit, target: "context" } : undefined,
  };
}

export function machineStates(m: SmMachine): string[] {
  const set = new Set<string>();
  set.add(m.initial);
  (m.terminal ?? []).forEach((s) => set.add(s));
  for (const t of m.transitions) {
    fromList(t.from).forEach((s) => set.add(s));
    set.add(t.to);
  }
  return [...set];
}
