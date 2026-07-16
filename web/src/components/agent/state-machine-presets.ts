// Starter state machines (all validated to compile). A state machine is the agent's
// tool-call-ordering + system-reminder engine: transitions gate/observe tool calls, `emit`
// injects a system reminder, `on_violation` denies/warns. These presets cover the three
// canonical shapes — a hard ordering gate, and two periodic reminders.

export interface SmEmit {
  target?: "system" | "suffix_system" | "session" | "conversation";
  content: string;
  cooldown_turns?: number;
  role?: "user" | "assistant";
}
export interface SmViolation {
  action: string; // deny | warn | ...
  reason?: string;
}
export interface SmTransition {
  on: string;
  from: string | string[];
  to: string;
  when?: string | { status?: string; content?: string };
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
  machine: SmMachine;
}

export const PRESETS: Preset[] = [
  {
    key: "read-before-write",
    label: "Read before write",
    labelZh: "先读后写",
    hint: "Deny a Write to a file the agent hasn't Read first (keyed per file).",
    hintZh: "对没先 Read 过的文件拒绝 Write(按文件分别追踪)。",
    machine: {
      // The built-in `read`/`write` tools take a `path` arg (match that, not `file_path`).
      name: "read-before-write",
      scope: "thread",
      key: "{path}",
      initial: "unread",
      terminal: ["written"],
      transitions: [
        { on: 'read(path ~ "*")', from: ["unread", "written", "read"], to: "read" },
        {
          on: 'write(path ~ "*")',
          from: "read",
          to: "written",
          on_violation: { action: "deny", reason: "Read {path} before writing it." },
        },
      ],
    },
  },
  {
    key: "background-task-reminder",
    label: "Background-task reminder",
    labelZh: "后台任务提醒",
    hint: "Every few turns, remind the agent a background task may still be running.",
    hintZh: "每隔几轮提醒 agent:后台任务可能还在跑,先看它的状态。",
    machine: {
      name: "background-task",
      scope: "run",
      initial: "active",
      transitions: [
        {
          on: "*",
          from: "active",
          to: "active",
          emit: {
            target: "suffix_system",
            content: "A background task may still be running — check its status/output before you finish.",
            cooldown_turns: 3,
          },
        },
      ],
    },
  },
  {
    key: "todo-reminder",
    label: "Todo reminder",
    labelZh: "待办提醒",
    hint: "Periodically nudge the agent to keep its todo list current.",
    hintZh: "周期性提醒 agent 保持待办清单更新。",
    machine: {
      name: "todo",
      scope: "run",
      initial: "active",
      transitions: [
        {
          on: "*",
          from: "active",
          to: "active",
          emit: {
            target: "suffix_system",
            content: "Keep your todo list current: mark finished items done and add new tasks as they arise.",
            cooldown_turns: 5,
          },
        },
      ],
    },
  },
];

/** Normalize a machine's `from` (string | string[]) to a list. */
export function fromList(from: string | string[]): string[] {
  return Array.isArray(from) ? from : [from];
}

/** All distinct states a machine references (initial + terminal + every from/to). */
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
