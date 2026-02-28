"use client";

import { ComponentProps, CSSProperties, useEffect, useMemo, useState } from "react";
import { CopilotKit } from "@copilotkit/react-core";
import {
  CopilotChat,
  UseAgentUpdate,
  useAgent,
  useConfigureSuggestions,
  useDefaultRenderTool,
  useFrontendTool,
  useHumanInTheLoop,
} from "@copilotkit/react-core/v2";
import { z } from "zod";
import { StarterNavTabs } from "@/components/starter-nav-tabs";
import { RecommendedActions } from "@/components/recommended-actions";
import {
  RECOMMENDED_ACTIONS,
  V2_RECOMMENDED_SUGGESTIONS,
} from "@/lib/recommended-actions";
import { useMainThreadId } from "@/lib/starter-thread";
import { StarterSplitScreen } from "@/components/starter-split-screen";
import { SharedStatePanel } from "@/components/shared-state-panel";
import { getMcpUiContent, McpAppFrame } from "@/components/mcp-app-frame";

type TodoStatus = "pending" | "completed";

type Todo = {
  id: string;
  title: string;
  description: string;
  emoji: string;
  status: TodoStatus;
};

const EMOJI_OPTIONS = ["✅", "🔥", "🎯", "💡", "🚀"];
const CHART_COLORS = ["#3b82f6", "#10b981", "#f59e0b", "#ef4444", "#8b5cf6"];
const toolCardClass =
  "mt-3 rounded-xl border border-slate-300 bg-white/90 p-3 text-slate-900 shadow-[0_10px_24px_rgba(15,23,42,0.08)]";
const buttonClass =
  "rounded-lg border border-slate-300 bg-white px-3 py-1.5 text-sm font-semibold text-slate-800 transition hover:-translate-y-px hover:border-slate-400 hover:bg-slate-50";
const inputClass =
  "w-full rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm text-slate-900 outline-none ring-cyan-300 transition focus:ring-2";

const todoSchema = z.object({
  id: z.string(),
  title: z.string(),
  description: z.string(),
  emoji: z.string(),
  status: z.enum(["pending", "completed"]),
});

const pieChartSchema = z.object({
  title: z.string().optional(),
  items: z
    .array(
      z.object({
        label: z.string(),
        value: z.number().nonnegative(),
      }),
    )
    .min(1),
});

const barChartSchema = z.object({
  title: z.string().optional(),
  items: z
    .array(
      z.object({
        label: z.string(),
        value: z.number().nonnegative(),
      }),
    )
    .min(1),
});

const scheduleTimeSchema = z.object({
  reasonForScheduling: z.string(),
  meetingDuration: z.number().int().positive(),
});

function readStateObject(input: unknown): Record<string, unknown> {
  return input && typeof input === "object" ? (input as Record<string, unknown>) : {};
}

function normalizeTodos(input: unknown): Todo[] {
  if (!Array.isArray(input)) return [];

  const out: Todo[] = [];
  for (const value of input) {
    const parsed = todoSchema.safeParse(value);
    if (!parsed.success) continue;
    out.push(parsed.data);
  }
  return out;
}

function createTodo(index: number, title?: string): Todo {
  return {
    id: crypto.randomUUID(),
    title: title && title.trim().length > 0 ? title : `Task ${index + 1}`,
    description: "Describe next step",
    emoji: EMOJI_OPTIONS[index % EMOJI_OPTIONS.length],
    status: "pending",
  };
}

function nextEmoji(current: string): string {
  const index = EMOJI_OPTIONS.indexOf(current);
  if (index < 0) return EMOJI_OPTIONS[0];
  return EMOJI_OPTIONS[(index + 1) % EMOJI_OPTIONS.length];
}

function ToolReasoning({
  name,
  args,
  status,
}: {
  name: string;
  args: unknown;
  status: string;
}) {
  const argEntries = args && typeof args === "object" ? Object.entries(args) : [];
  return (
    <div className={toolCardClass}>
      <div className="flex items-center justify-between gap-2">
        <strong>{name}</strong>
        <span className="text-xs text-slate-500">{status}</span>
      </div>
      {argEntries.length > 0 && (
        <ul className="mt-2 list-disc space-y-1 pl-5 text-sm text-slate-700">
          {argEntries.map(([key, value]) => (
            <li key={key}>
              {key}: {typeof value === "string" ? value : JSON.stringify(value)}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

function PieChartCard({
  title,
  items,
  status,
}: {
  title?: string;
  items: Array<{ label: string; value: number }>;
  status: string;
}) {
  const total = items.reduce((sum, item) => sum + item.value, 0);
  const gradient = useMemo(() => {
    if (total <= 0) return "conic-gradient(#cbd5e1 0 100%)";
    let offset = 0;
    const segments = items.map((item, index) => {
      const percent = (item.value / total) * 100;
      const start = offset;
      offset += percent;
      return `${CHART_COLORS[index % CHART_COLORS.length]} ${start}% ${offset}%`;
    });
    return `conic-gradient(${segments.join(", ")})`;
  }, [items, total]);

  return (
    <div data-testid="canvas-pie-chart-card" className={toolCardClass}>
      <strong>
        {title || "Pie Chart"} ({status})
      </strong>
      <div className="mt-3 flex gap-3">
        <div
          className="h-28 w-28 shrink-0 rounded-full border border-slate-300"
          style={{ background: gradient }}
        />
        <ul className="m-0 list-disc space-y-1 pl-5 text-sm text-slate-700">
          {items.map((item, index) => (
            <li key={`${item.label}-${index}`}>
              <span
                className="mr-1.5 inline-block h-2 w-2 rounded-full"
                style={{ backgroundColor: CHART_COLORS[index % CHART_COLORS.length] }}
              />
              {item.label}: {item.value}
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}

function BarChartCard({
  title,
  items,
  status,
}: {
  title?: string;
  items: Array<{ label: string; value: number }>;
  status: string;
}) {
  const max = Math.max(...items.map((item) => item.value), 1);
  return (
    <div data-testid="canvas-bar-chart-card" className={toolCardClass}>
      <strong>
        {title || "Bar Chart"} ({status})
      </strong>
      <div className="mt-3 grid gap-2">
        {items.map((item, index) => (
          <div key={`${item.label}-${index}`} className="grid gap-1">
            <div className="text-sm text-slate-700">
              {item.label}: {item.value}
            </div>
            <div className="h-2.5 overflow-hidden rounded-full bg-slate-200">
              <div
                className="h-full"
                style={{
                  width: `${(item.value / max) * 100}%`,
                  backgroundColor: CHART_COLORS[index % CHART_COLORS.length],
                }}
              />
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

function MeetingTimePicker({
  args,
  status,
  respond,
}: {
  args: Partial<z.infer<typeof scheduleTimeSchema>>;
  status: string;
  respond?: (result: unknown) => Promise<void>;
}) {
  const [selectedTime, setSelectedTime] = useState("Tomorrow 10:00");
  const reason =
    typeof args.reasonForScheduling === "string" ? args.reasonForScheduling : "Schedule a meeting";
  const duration = typeof args.meetingDuration === "number" ? args.meetingDuration : 30;

  if (status === "complete") {
    return (
      <div className={`${toolCardClass} border-green-200 bg-green-50`}>
        <strong>Meeting Request Completed</strong>
      </div>
    );
  }

  return (
    <div data-testid="canvas-hitl-card" className={toolCardClass}>
      <strong>Schedule Meeting ({status})</strong>
      <p className="mt-2 text-sm text-slate-700">Reason: {reason}</p>
      <p className="text-sm text-slate-700">Duration: {duration} min</p>
      <select
        data-testid="canvas-hitl-time"
        value={selectedTime}
        onChange={(event) => setSelectedTime(event.target.value)}
        className={`${inputClass} mt-2`}
        disabled={status !== "executing"}
      >
        <option>Tomorrow 10:00</option>
        <option>Tomorrow 14:00</option>
        <option>Friday 09:30</option>
      </select>
      <div className="mt-3 flex flex-wrap gap-2">
        <button
          data-testid="canvas-hitl-approve"
          className="rounded-lg border border-green-300 bg-green-50 px-3 py-1.5 text-sm font-semibold text-green-800 transition hover:-translate-y-px hover:border-green-400 hover:bg-green-100 disabled:cursor-not-allowed disabled:opacity-60 disabled:hover:translate-y-0"
          disabled={!respond || status !== "executing"}
          onClick={() => respond?.({ approved: true, selectedTime })}
        >
          Approve
        </button>
        <button
          data-testid="canvas-hitl-deny"
          className="rounded-lg border border-red-300 bg-red-50 px-3 py-1.5 text-sm font-semibold text-red-800 transition hover:-translate-y-px hover:border-red-400 hover:bg-red-100 disabled:cursor-not-allowed disabled:opacity-60 disabled:hover:translate-y-0"
          disabled={!respond || status !== "executing"}
          onClick={() => respond?.({ approved: false })}
        >
          Deny
        </button>
      </div>
    </div>
  );
}

type V2WelcomeScreenProps = ComponentProps<typeof CopilotChat.View.WelcomeScreen>;

function CanvasWelcomeScreen({ input, suggestionView }: V2WelcomeScreenProps) {
  return (
    <div className="starter-chat-v2-welcome flex h-full min-h-0 flex-col bg-white">
      <div className="starter-chat-v2-initial p-4 pb-0">
        <p className="m-0 rounded-xl border border-slate-200 bg-slate-50 px-3 py-2 text-base leading-relaxed text-slate-700">
          Try: add todos, call pieChart/barChart, then scheduleTime.
        </p>
      </div>
      <div className="starter-chat-v2-suggestions-wrap px-3 py-2">{suggestionView}</div>
      {input}
      <p className="starter-chat-v2-footer hidden">Powered by CopilotKit</p>
    </div>
  );
}

function TodoBoard({
  todos,
  onChange,
}: {
  todos: Todo[];
  onChange: (next: Todo[]) => void;
}) {
  const pending = todos.filter((todo) => todo.status === "pending");
  const completed = todos.filter((todo) => todo.status === "completed");

  const updateTodo = (id: string, patch: Partial<Todo>) => {
    onChange(todos.map((todo) => (todo.id === id ? { ...todo, ...patch } : todo)));
  };

  const deleteTodo = (id: string) => {
    onChange(todos.filter((todo) => todo.id !== id));
  };

  const addTodo = () => {
    onChange([...todos, createTodo(todos.length)]);
  };

  const renderColumn = (title: string, list: Todo[]) => (
    <div className="min-w-0 flex-1 rounded-2xl border border-slate-200 bg-white/90 p-3">
      <h3 className="text-base font-semibold text-slate-900">{title}</h3>
      <div className="mt-3 grid gap-2">
        {list.length === 0 && (
          <div className="rounded-lg border border-dashed border-slate-300 px-3 py-2 text-sm text-slate-500">
            No tasks
          </div>
        )}
        {list.map((todo) => (
          <div
            key={todo.id}
            data-testid="canvas-todo-card"
            className="rounded-xl border border-slate-300 bg-slate-50 p-2.5"
          >
            <div className="flex flex-wrap items-center gap-2">
              <button
                data-testid="canvas-toggle-status"
                className={buttonClass}
                onClick={() =>
                  updateTodo(todo.id, {
                    status: todo.status === "pending" ? "completed" : "pending",
                  })
                }
              >
                {todo.status === "pending" ? "Mark Done" : "Reopen"}
              </button>
              <button
                data-testid="canvas-emoji"
                className={buttonClass}
                onClick={() => updateTodo(todo.id, { emoji: nextEmoji(todo.emoji) })}
              >
                {todo.emoji}
              </button>
              <button
                data-testid="canvas-delete-todo"
                className="rounded-lg border border-red-300 bg-red-50 px-3 py-1.5 text-sm font-semibold text-red-800 transition hover:-translate-y-px hover:border-red-400 hover:bg-red-100"
                onClick={() => deleteTodo(todo.id)}
              >
                Delete
              </button>
            </div>
            <input
              data-testid="canvas-title-input"
              value={todo.title}
              onChange={(event) => updateTodo(todo.id, { title: event.target.value })}
              className={`${inputClass} mt-2`}
            />
            <textarea
              data-testid="canvas-description-input"
              value={todo.description}
              onChange={(event) => updateTodo(todo.id, { description: event.target.value })}
              className={`${inputClass} mt-2 min-h-[66px]`}
            />
          </div>
        ))}
      </div>
    </div>
  );

  return (
    <div className="mt-3">
      <div className="flex flex-wrap items-center gap-2">
        <button data-testid="canvas-add-todo" className={buttonClass} onClick={addTodo}>
          Add Todo
        </button>
        <p data-testid="canvas-pending-count" className="m-0 text-sm text-slate-700">
          Pending: {pending.length}
        </p>
        <p data-testid="canvas-completed-count" className="m-0 text-sm text-slate-700">
          Completed: {completed.length}
        </p>
      </div>
      <div className="mt-3 flex flex-col gap-3 lg:flex-row">
        {renderColumn("To Do", pending)}
        {renderColumn("Done", completed)}
      </div>
    </div>
  );
}

function CanvasV2Demo() {
  const [themeMode, setThemeMode] = useState<"light" | "dark">("light");
  const { agent } = useAgent({
    agentId: "default",
    updates: [UseAgentUpdate.OnStateChanged, UseAgentUpdate.OnRunStatusChanged],
  });

  const stateObject = readStateObject(agent.state);
  const [todos, setLocalTodos] = useState<Todo[]>(() => normalizeTodos(stateObject.todos));

  useEffect(() => {
    setLocalTodos(normalizeTodos(readStateObject(agent.state).todos));
  }, [agent.state]);

  const setTodos = (nextTodos: Todo[]) => {
    const currentState = readStateObject(agent.state);
    setLocalTodos(nextTodos);
    agent.setState({
      ...currentState,
      todos: nextTodos,
    });
  };

  useConfigureSuggestions(
    {
      suggestions: V2_RECOMMENDED_SUGGESTIONS,
      available: "always",
    },
    [],
  );

  useFrontendTool(
    {
      name: "toggleCanvasTheme",
      description: "Toggle canvas theme between light and dark mode.",
      parameters: z.object({}),
      handler: async () => {
        setThemeMode((prev) => (prev === "dark" ? "light" : "dark"));
      },
    },
    [],
  );

  useFrontendTool(
    {
      name: "replaceCanvasTodos",
      description: "Replace the entire todo list in shared state.",
      parameters: z.object({
        todos: z.array(todoSchema),
      }),
      handler: async ({ todos: nextTodos }) => {
        setTodos(nextTodos);
      },
    },
    [agent],
  );

  useFrontendTool(
    {
      name: "pieChart",
      description: "Render a pie chart card in chat.",
      parameters: pieChartSchema,
      handler: async () => "pie chart rendered",
      render: ({ args, status }) => (
        <PieChartCard title={args.title} items={args.items ?? []} status={status} />
      ),
    },
    [],
  );

  useFrontendTool(
    {
      name: "barChart",
      description: "Render a bar chart card in chat.",
      parameters: barChartSchema,
      handler: async () => "bar chart rendered",
      render: ({ args, status }) => (
        <BarChartCard title={args.title} items={args.items ?? []} status={status} />
      ),
    },
    [],
  );

  useDefaultRenderTool(
    {
      render: ({ name, args, status, result }) => {
        const mcpUi = getMcpUiContent(result);
        if (mcpUi) {
          return (
            <div>
              <ToolReasoning name={name} args={args} status={status} />
              <McpAppFrame
                content={mcpUi.content}
                mimeType={mcpUi.mimeType}
                resourceUri={mcpUi.resourceUri}
              />
            </div>
          );
        }
        return <ToolReasoning name={name} args={args} status={status} />;
      },
    },
    [],
  );

  useHumanInTheLoop(
    {
      name: "scheduleTime",
      description: "Ask user to approve and pick a meeting slot.",
      parameters: scheduleTimeSchema,
      render: ({ args, status, respond }) => (
        <MeetingTimePicker args={args} status={status} respond={respond} />
      ),
    },
    [],
  );

  const isDark = themeMode === "dark";
  const surface = isDark ? "#0f172a" : "#f8fafc";
  const text = isDark ? "#e2e8f0" : "#0f172a";
  const pageStyle = {
    backgroundColor: surface,
    color: text,
    ["--page-accent" as "--page-accent"]: isDark ? "#1d4ed8" : "#0891b2",
  } as CSSProperties;
  const sharedTodos = todos.map((todo) =>
    `${todo.status === "completed" ? "Done" : "Todo"}: ${todo.title}`,
  );
  const subtitleClass = isDark ? "text-slate-200" : "text-slate-700";
  const hintClass = isDark ? "text-slate-300" : "text-slate-600";
  const boardCardClass = isDark
    ? "rounded-xl border border-slate-600 bg-slate-900/70 p-3"
    : "rounded-xl border border-slate-200 bg-white/75 p-3";

  const leftPane = (
    <div className="mx-auto max-w-[1040px]">
      <div className="inline-flex items-center gap-2 rounded-full border border-white/50 bg-slate-900/30 px-3 py-1 text-xs font-semibold tracking-wide text-slate-100">
        CopilotKit x tirea starter
      </div>
      <h1 data-testid="page-title" className="mt-3 text-4xl font-bold tracking-tight text-white md:text-5xl">
        with-tirea canvas
      </h1>
      <p className={`mt-2 max-w-[760px] text-sm md:text-base ${subtitleClass}`}>
        v2 canvas demo with shared-state board, structured HITL, and tool-rendered UI.
      </p>
      <StarterNavTabs mode="v2" />
      <RecommendedActions title="Recommended Actions" actions={RECOMMENDED_ACTIONS} defaultStack="v2" />
      <section className="mt-4 grid gap-3 rounded-2xl border border-white/60 bg-white/80 p-5 text-slate-900 shadow-[0_20px_45px_rgba(15,23,42,0.14)] backdrop-blur">
        <div className={boardCardClass}>
          <div className="mb-2 inline-flex items-center rounded-full border border-blue-200 bg-blue-50 px-2 py-0.5 text-xs font-bold text-blue-700">
            v2 Agent Status
          </div>
          <p data-testid="canvas-agent-running" className={`mt-0 text-sm ${hintClass}`}>
            Agent running: {agent.isRunning ? "yes" : "no"}
          </p>
          <p data-testid="canvas-prompt" className={`mt-2 text-sm ${hintClass}`}>
            Prompt: "Add two todos, render a pieChart, then scheduleTime for 45 minutes."
          </p>
        </div>
        <SharedStatePanel
          label="Shared State"
          title="1) Shared State (useAgent)"
          prompt="Add two todos about AG-UI contract verification."
          todos={sharedTodos}
          defaultInput="Task from shared-state panel"
          onAddTodo={(text) => {
            setTodos([...todos, createTodo(todos.length, text)]);
          }}
          onClearTodos={() => {
            setTodos([]);
          }}
        />
        <div className={boardCardClass}>
          <div className="mb-2 inline-flex items-center rounded-full border border-blue-200 bg-blue-50 px-2 py-0.5 text-xs font-bold text-blue-700">
            Shared State Canvas
          </div>
          <section className="mt-2">
            <TodoBoard todos={todos} onChange={setTodos} />
          </section>
        </div>
      </section>
    </div>
  );

  return (
    <StarterSplitScreen
      rootTestId="canvas-root"
      pageStyle={pageStyle}
      chatHint="Try: add todos, call pieChart/barChart, then scheduleTime."
      left={leftPane}
      chat={
        <CopilotChat
          className="starter-chat-unified starter-chat-v2 h-full"
          labels={{
            welcomeMessageText: "Try: add todos, call pieChart/barChart, then scheduleTime.",
          }}
          chatView={{
            welcomeScreen: CanvasWelcomeScreen,
            messageView: {
              className: "starter-chat-v2-messages",
            },
            suggestionView: {
              className: "starter-chat-v2-suggestions",
            },
            input: {
              className: "starter-chat-v2-input",
            },
          }}
        />
      }
    />
  );
}

export default function CanvasPage() {
  const { threadId } = useMainThreadId();

  return (
    <CopilotKit runtimeUrl="/api/copilotkit" agent="default" threadId={threadId}>
      <CanvasV2Demo />
    </CopilotKit>
  );
}
