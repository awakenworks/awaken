import { type CSSProperties, useMemo, useState } from "react";
import { useSearchParams } from "react-router";
import { SplitScreen } from "@/components/layout/split-screen";
import { NavTabs, type PlaygroundMode } from "@/components/layout/nav-tabs";
import { ChatPanel } from "@/components/chat/chat-panel";
import { SharedStatePanel } from "@/components/demo/shared-state-panel";
import { TodoBoard, type Todo, createTodo } from "@/components/demo/todo-board";
import { RECOMMENDED_ACTIONS } from "@/lib/recommended-actions";
import { getSessionId } from "@/lib/session";
import { useThreads } from "@/hooks/use-threads";

const sectionCardClass = "rounded-xl border border-slate-200 bg-white/80 p-4";
const sectionLabelClass =
  "mb-2 inline-flex items-center rounded-full border border-blue-200 bg-blue-50 px-2 py-0.5 text-xs font-bold text-blue-700";

export function PlaygroundPage() {
  const [searchParams] = useSearchParams();
  const [mode, setMode] = useState<PlaygroundMode>("conversation");
  const [canvasTheme, setCanvasTheme] = useState<"light" | "dark">("light");
  const [toolTodos, setToolTodos] = useState<string[]>([
    "Review integration flow",
    "Run local smoke check",
  ]);
  const [canvasTodos, setCanvasTodos] = useState<Todo[]>([]);
  const [toolsThreadId] = useState(() => `tools-${getSessionId()}`);
  const [canvasThreadId] = useState(() => `canvas-${getSessionId()}`);
  const { threads, activeThreadId, setActiveThreadId, createThread, loaded } = useThreads();

  const pageStyle = useMemo(() => {
    if (mode === "canvas" && canvasTheme === "dark") {
      return { backgroundColor: "#0f172a", color: "#e2e8f0" } as CSSProperties;
    }
    return { backgroundColor: "#e2e8f0", color: "#0f172a" } as CSSProperties;
  }, [mode, canvasTheme]);

  const chatTheme = mode === "canvas" ? canvasTheme : "light";
  const agentId = searchParams.get("agentId")?.trim() || "default";
  const chatThreadId =
    mode === "conversation"
      ? activeThreadId
      : mode === "tools"
        ? toolsThreadId
        : canvasThreadId;

  const sharedCanvasTodos = canvasTodos.map(
    (todo) => `${todo.status === "completed" ? "Done" : "Todo"}: ${todo.title}`,
  );

  const leftPane = (
    <div className="mx-auto max-w-[1040px]">
      <div className="inline-flex items-center gap-2 rounded-full border border-slate-300 bg-white px-3 py-1 text-xs font-semibold tracking-wide text-slate-700">
        tirea + AI SDK
      </div>
      <h1
        data-testid="page-title"
        className="mt-3 text-4xl font-bold tracking-tight text-slate-900 md:text-5xl"
      >
        AI SDK Framework Integration Playground
      </h1>
      <p className="mt-2 max-w-[820px] text-sm text-slate-700 md:text-base">
        One-page demo for AI SDK integration with tirea runtime. Includes persisted
        threads and backend tools (weather, stock, notes, approvals, ask-user).
        TODO board is only a UI visualization effect for state presentation.
      </p>
      <NavTabs mode={mode} onModeChange={setMode} />

      {mode === "conversation" && (
        <div className="mt-4 grid gap-3 rounded-2xl border border-slate-300 bg-white/90 p-5 text-slate-900 shadow-[0_20px_45px_rgba(15,23,42,0.12)]">
          <section className={sectionCardClass}>
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div>
                <div className={sectionLabelClass}>Persisted Threads</div>
                <h2 className="text-xl font-semibold text-slate-900">Conversation State</h2>
                <p data-testid="active-thread" className="mt-1 text-sm text-slate-700">
                  Active thread:{" "}
                  {activeThreadId ? `${activeThreadId.slice(0, 16)}...` : "none"}
                </p>
              </div>
              <button
                data-testid="thread-create"
                onClick={createThread}
                className="min-w-32 rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm font-semibold text-slate-800 transition hover:-translate-y-px hover:border-slate-400 hover:bg-slate-50"
              >
                + New Thread
              </button>
            </div>
            <p data-testid="thread-prompt" className="mt-2 text-sm text-slate-600">
              Switch threads to load backend-persisted message history.
            </p>
            <div
              data-testid="thread-list"
              className="mt-3 grid grid-cols-[repeat(auto-fill,minmax(170px,1fr))] gap-2"
            >
              {threads.map((thread) => (
                <button
                  key={thread.id}
                  data-testid={`thread-item-${thread.id}`}
                  onClick={() => setActiveThreadId(thread.id)}
                  className={`rounded-lg border px-3 py-2 text-left transition ${
                    activeThreadId === thread.id
                      ? "border-cyan-700 bg-cyan-50 text-cyan-900"
                      : "border-slate-200 bg-white text-slate-900 hover:-translate-y-px hover:border-slate-300"
                  }`}
                >
                  <div className="text-sm font-semibold">{thread.title}</div>
                  <div className="text-xs text-slate-500">{thread.id.slice(0, 12)}</div>
                </button>
              ))}
            </div>
          </section>
        </div>
      )}

      {mode === "tools" && (
        <div className="mt-4 grid gap-3 rounded-2xl border border-slate-300 bg-white/90 p-5 text-slate-900 shadow-[0_20px_45px_rgba(15,23,42,0.12)]">
          <SharedStatePanel
            label="Local State"
            title="Tools Demo"
            prompt="Use weather/stock/note tools and observe structured tool cards."
            todos={toolTodos}
            defaultInput="Write a regression test"
            onAddTodo={(text) => setToolTodos((prev) => [...prev, text])}
            onClearTodos={() => setToolTodos([])}
          />

          <section className={sectionCardClass}>
            <div className={sectionLabelClass}>Backend Tools</div>
            <h3 className="text-lg font-semibold text-slate-900">
              Weather / Stock / Notes / Approval / AskUser
            </h3>
            <p data-testid="backend-tools-prompt" className="mt-2 text-sm text-slate-600">
              Try: "What's the weather in San Francisco?" or "Show AAPL stock price."
            </p>
            <p data-testid="approval-prompt" className="mt-1 text-sm text-slate-500">
              Permission and ask-user dialogs are part of the integration contract.
            </p>
          </section>
        </div>
      )}

      {mode === "canvas" && (
        <div className="mt-4 grid gap-3 rounded-2xl border border-slate-300 bg-white/90 p-5 text-slate-900 shadow-[0_20px_45px_rgba(15,23,42,0.12)]">
          <section className={sectionCardClass}>
            <div className={sectionLabelClass}>Canvas Controls</div>
            <button
              data-testid="toggle-theme"
              onClick={() =>
                setCanvasTheme((current) => (current === "dark" ? "light" : "dark"))
              }
              className="rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm font-semibold text-slate-800 transition hover:-translate-y-px hover:border-slate-400 hover:bg-slate-50"
            >
              Toggle Theme ({canvasTheme})
            </button>
          </section>

          <SharedStatePanel
            label="Shared State"
            title="Shared State (UI effect only)"
            prompt="TODO board here demonstrates UI state only, not backend capability limits."
            todos={sharedCanvasTodos}
            defaultInput="Visual task item"
            onAddTodo={(text) =>
              setCanvasTodos((prev) => [...prev, createTodo(prev.length, text)])
            }
            onClearTodos={() => setCanvasTodos([])}
          />

          <section className={sectionCardClass}>
            <div className={sectionLabelClass}>Todo Board</div>
            <TodoBoard todos={canvasTodos} onChange={setCanvasTodos} />
          </section>
        </div>
      )}
    </div>
  );

  const chat = !loaded && mode === "conversation"
    ? (
        <div className="flex h-full items-center justify-center text-sm text-slate-400">
          Loading threads...
        </div>
      )
    : !chatThreadId
      ? (
          <div className="flex h-full items-center justify-center text-sm text-slate-400">
            Create or select a thread to start chatting.
          </div>
        )
      : (
          <ChatPanel
            key={`${mode}-${chatThreadId}`}
            threadId={chatThreadId}
            agentId={agentId}
            themeMode={chatTheme}
            layout={mode === "conversation" ? "conversation" : "default"}
            recommendedActions={RECOMMENDED_ACTIONS}
          />
        );

  return (
    <SplitScreen
      rootTestId="playground-root"
      pageStyle={pageStyle}
      rootTextClass={mode === "canvas" && canvasTheme === "dark" ? "text-slate-100" : "text-slate-900"}
      chatHint="OpenAI/DeepSeek style conversation panel with AI SDK streaming."
      chatTheme={chatTheme}
      left={leftPane}
      chat={chat}
    />
  );
}
