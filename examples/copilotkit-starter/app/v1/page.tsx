"use client";

import { CSSProperties, useState } from "react";
import { CopilotChat } from "@copilotkit/react-ui";
import {
  CopilotKit,
  useCopilotAction,
  useCoAgent,
  useDefaultTool,
  useRenderToolCall,
} from "@copilotkit/react-core";
import { z } from "zod";
import { StarterNavTabs } from "@/components/starter-nav-tabs";
import { DefaultToolComponent } from "@/components/default-tool-ui";
import { RecommendedActions } from "@/components/recommended-actions";
import { WeatherCard } from "@/components/weather";
import {
  JsonRenderPanel,
  OpenUIPanel,
  A2UIPanel,
} from "@/components/generative-ui-renderers";
import {
  RECOMMENDED_ACTIONS,
  V1_RECOMMENDED_SUGGESTIONS,
} from "@/lib/recommended-actions";
import { useMainThreadId } from "@/lib/starter-thread";
import { StarterSplitScreen } from "@/components/starter-split-screen";
import { SharedStatePanel } from "@/components/shared-state-panel";
import { McpAppFrame } from "@/components/mcp-app-frame";
import { useStarterAgentId } from "@/lib/agent-id";

type DemoState = {
  todos: string[];
};

const panelCardClass =
  "rounded-xl border border-slate-200 bg-white/75 p-3";
const sectionLabelClass =
  "mb-2 inline-flex items-center rounded-full border border-blue-200 bg-blue-50 px-2 py-0.5 text-xs font-bold text-blue-700";
const subtleHintClass = "mt-2 text-sm text-slate-600";
const buttonClass =
  "rounded-lg border border-slate-300 bg-white px-3 py-2 text-sm font-semibold text-slate-800 transition hover:-translate-y-px hover:border-slate-400 hover:bg-slate-50";

const releaseChecklistSchema = z
  .object({
    items: z.array(z.string().trim().min(1)).min(1),
  })
  .strict();

function ApprovalCard({
  onApprove,
  onDeny,
}: {
  onApprove: () => void;
  onDeny: () => void;
}) {
  return (
    <div className="rounded-lg border border-slate-300 bg-white p-3">
      <p className="m-0 text-sm text-slate-800">Clear all todos?</p>
      <div className="mt-3 flex flex-wrap items-center gap-2">
        <button
          data-testid="hitl-approve"
          onClick={onApprove}
          className="rounded-lg border border-green-300 bg-green-50 px-3 py-1.5 text-sm font-semibold text-green-800 transition hover:-translate-y-px hover:border-green-400 hover:bg-green-100"
        >
          Approve
        </button>
        <button
          data-testid="hitl-deny"
          onClick={onDeny}
          className="rounded-lg border border-red-300 bg-red-50 px-3 py-1.5 text-sm font-semibold text-red-800 transition hover:-translate-y-px hover:border-red-400 hover:bg-red-100"
        >
          Deny
        </button>
      </div>
    </div>
  );
}

function ReleaseChecklistCard({
  status,
  items,
  cardTestId = "generative-ui-card",
  checklistTestId = "generative-ui-checklist",
  itemTestId = "generative-ui-checklist-item",
}: {
  status: string;
  items: string[];
  cardTestId?: string;
  checklistTestId?: string;
  itemTestId?: string;
}) {
  return (
    <div
      data-testid={cardTestId}
      className="mt-3 rounded-xl border border-slate-300 bg-white/90 p-3 text-slate-900 shadow-[0_10px_24px_rgba(15,23,42,0.08)]"
    >
      <strong>Generative UI: Release Checklist ({status})</strong>
      <div data-testid={checklistTestId} className="mt-3 grid gap-2">
        {items.map((item, index) => (
          <div
            key={`${item}-${index}`}
            data-testid={itemTestId}
            className="flex items-center gap-2 rounded-lg border border-slate-200 bg-slate-50 px-2 py-1.5"
          >
            <span aria-hidden className="font-bold text-green-600">
              [ ]
            </span>
            <span className="text-sm text-slate-800">{item}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

function BaseStarterDemo({ agentId }: { agentId: "default" | "permission" | "stopper" }) {
  const [themeColor, setThemeColor] = useState("#3b82f6");
  const [lastFrontendAction, setLastFrontendAction] = useState("none");
  const [hitlStatus, setHitlStatus] = useState("pending");
  const [showLocalHitl, setShowLocalHitl] = useState(false);
  const [localChecklistPreview, setLocalChecklistPreview] = useState<string[] | null>(
    null,
  );
  const [showMcpAppPreview, setShowMcpAppPreview] = useState(false);
  const [rendererPreview, setRendererPreview] = useState<
    "none" | "json-render" | "openui" | "a2ui"
  >("none");

  const { state, setState } = useCoAgent<DemoState>({
    name: "default",
    initialState: { todos: ["Review awaken integration", "Run AG-UI smoke test"] },
  });

  useCopilotAction({
    name: "setThemeColor",
    description: "Set the page theme color",
    parameters: [{ name: "color", type: "string", required: true }],
    handler: async ({ color }: { color: string }) => {
      setThemeColor(color);
      setLastFrontendAction(`setThemeColor(${color})`);
    },
  });

  useRenderToolCall(
    {
      name: "get_weather",
      parameters: [
        {
          name: "location",
          description: "The location to get the weather for.",
          required: true,
        },
      ],
      render: ({ args }) => (
        <WeatherCard themeColor={themeColor} location={(args?.location as string) ?? ""} />
      ),
    },
    [themeColor],
  );

  useDefaultTool(
    {
      render: (props) => <DefaultToolComponent themeColor={themeColor} {...props} />,
    },
    [themeColor],
  );

  useCopilotAction({
    name: "showReleaseChecklist",
    description:
      "Render a release checklist card in chat. Always call this tool for release checklist requests.",
    parameters: [
      {
        name: "items",
        type: "string[]",
        description: "Checklist items as an array of strings, e.g. [\"run tests\", \"tag release\"]",
        required: true,
      },
    ],
    handler: async (args: { items: string[] }) => {
      const parsed = releaseChecklistSchema.safeParse(args);
      if (!parsed.success) {
        throw new Error("showReleaseChecklist requires { items: string[] } with at least one item.");
      }
      return {
        rendered: true,
        itemCount: parsed.data.items.length,
      };
    },
    render: ({ args, status }) => {
      if (status !== "complete") {
        return (
          <div
            data-testid="generative-ui-card"
            className="mt-3 rounded-xl border border-slate-300 bg-white/90 p-3 text-slate-900 shadow-[0_10px_24px_rgba(15,23,42,0.08)]"
          >
            <strong>Generative UI: Release Checklist ({status})</strong>
            <p className="mt-2 text-sm text-slate-600">Preparing checklist...</p>
          </div>
        );
      }

      const parsed = releaseChecklistSchema.safeParse(args);
      if (!parsed.success) {
        return (
          <div
            data-testid="generative-ui-card-error"
            className="mt-3 rounded-xl border border-red-300 bg-red-50 p-3 text-red-900 shadow-[0_10px_24px_rgba(15,23,42,0.08)]"
          >
            <strong>Generative UI: Release Checklist (invalid-args)</strong>
            <p className="mt-2 text-sm">
              showReleaseChecklist expects `items: string[]` with at least one entry.
            </p>
          </div>
        );
      }

      const items = parsed.data.items;
      return <ReleaseChecklistCard status={status} items={items} />;
    },
  });

  useCopilotAction({
    name: "addTodo",
    description: "Append a todo item",
    parameters: [{ name: "text", type: "string", required: true }],
    handler: async ({ text }: { text: string }) => {
      setState({ ...state, todos: [...(state?.todos ?? []), text] });
      setLastFrontendAction(`addTodo(${text})`);
    },
  });

  useCopilotAction({
    name: "confirmClearTodos",
    description: "Ask user approval before clearing todos",
    parameters: [],
    renderAndWaitForResponse: ({ respond, status }) => {
      if (status === "complete") {
        return <p className="text-sm font-medium text-green-600">Todos action resolved.</p>;
      }
      return (
        <ApprovalCard
          onApprove={() => {
            setState({ ...state, todos: [] });
            setHitlStatus("approved");
            respond?.("approved");
          }}
          onDeny={() => {
            setHitlStatus("denied");
            respond?.("denied");
          }}
        />
      );
    },
  });

  const currentTodos = state?.todos ?? [];
  const pageStyle = {
    backgroundColor: themeColor,
    ["--page-accent" as "--page-accent"]: themeColor,
  } as CSSProperties;

  const leftPane = (
    <div className="mx-auto max-w-[1040px]">
      <div className="inline-flex items-center gap-2 rounded-full border border-white/50 bg-slate-900/30 px-3 py-1 text-xs font-semibold tracking-wide text-slate-100">
        CopilotKit x awaken starter
      </div>
      <h1 data-testid="page-title" className="mt-3 text-4xl font-bold tracking-tight text-white md:text-5xl">
        with-awaken
      </h1>
      <p className="mt-2 max-w-[760px] text-sm text-slate-100/95 md:text-base">
        CopilotKit starter for awaken (AG-UI endpoint).
      </p>
      <p data-testid="active-agent" className="mt-1 text-xs font-semibold text-slate-100/90">
        Active agent: {agentId}
      </p>
      <StarterNavTabs mode="v1" />
      <RecommendedActions title="Recommended Actions" actions={RECOMMENDED_ACTIONS} defaultStack="v1" />

      <div className="mt-4 grid gap-3 rounded-2xl border border-white/60 bg-white/80 p-5 text-slate-900 shadow-[0_20px_45px_rgba(15,23,42,0.14)] backdrop-blur">
        <SharedStatePanel
          label="Shared State"
          title="1) Shared State (useCoAgent)"
          prompt="Add a todo: verify AG-UI event ordering."
          todos={currentTodos}
          defaultInput="Write a Playwright smoke test"
          onAddTodo={(text) => {
            setState({ ...state, todos: [...currentTodos, text] });
            setLastFrontendAction(`localAddTodo(${text})`);
          }}
          onClearTodos={() => {
            setState({ ...state, todos: [] });
            setLastFrontendAction("localClearTodos()");
          }}
        />
        <section className={panelCardClass}>
          <div className={sectionLabelClass}>Frontend Actions</div>
          <h3 className="text-lg font-semibold text-slate-900">2) Frontend Actions (useCopilotAction)</h3>
          <p data-testid="frontend-actions-prompt" className={subtleHintClass}>
            Prompt: "Set theme color to #16a34a, then add todo: ship starter."
          </p>
          <p data-testid="frontend-action-last" className="mt-2 text-sm text-slate-700">
            Last action: {lastFrontendAction}
          </p>
          <div className="mt-3 flex flex-wrap items-center gap-2">
            <button
              data-testid="theme-blue"
              className={buttonClass}
              onClick={() => {
                setThemeColor("#3b82f6");
                setLastFrontendAction("localSetTheme(#3b82f6)");
              }}
            >
              Blue Theme
            </button>
            <button
              data-testid="theme-green"
              className="rounded-lg border border-green-400 bg-green-50 px-3 py-2 text-sm font-semibold text-green-800 transition hover:-translate-y-px hover:border-green-500 hover:bg-green-100"
              onClick={() => {
                setThemeColor("#16a34a");
                setLastFrontendAction("localSetTheme(#16a34a)");
              }}
            >
              Green Theme
            </button>
          </div>
        </section>
        <section className={panelCardClass}>
          <div className={sectionLabelClass}>Generative UI</div>
          <h3 className="text-lg font-semibold text-slate-900">3) Generative UI (render / useRenderToolCall)</h3>
          <p data-testid="generative-ui-prompt" className={subtleHintClass}>
            Prompt: "Call showReleaseChecklist with items: run tests, tag release."
          </p>
          <div
            data-testid="generative-ui-preview"
            className="mt-2 rounded-lg border border-dashed border-slate-500 bg-slate-50/90 p-3 text-sm text-slate-700"
          >
            Expected result: a checklist card is rendered inside Copilot chat.
          </div>
          <div className="mt-3 flex flex-wrap items-center gap-2">
            <button
              data-testid="generative-ui-local-preview-trigger"
              className={buttonClass}
              onClick={() => setLocalChecklistPreview(["run tests", "tag release"])}
            >
              Render Local Preview
            </button>
            <button
              data-testid="generative-ui-local-preview-clear"
              className={buttonClass}
              onClick={() => setLocalChecklistPreview(null)}
              disabled={!localChecklistPreview}
            >
              Clear Preview
            </button>
          </div>
          {localChecklistPreview ? (
            <ReleaseChecklistCard
              status="preview"
              items={localChecklistPreview}
              cardTestId="generative-ui-local-card"
              checklistTestId="generative-ui-local-checklist"
              itemTestId="generative-ui-local-checklist-item"
            />
          ) : null}
          <div className="mt-4 border-t border-slate-200 pt-3">
            <div className="text-xs font-semibold text-slate-500 mb-2">
              Official Renderer Previews
            </div>
            <div className="flex flex-wrap items-center gap-2">
              <button
                data-testid="renderer-json-render"
                className={buttonClass}
                onClick={() =>
                  setRendererPreview(
                    rendererPreview === "json-render" ? "none" : "json-render",
                  )
                }
              >
                JSON Render
              </button>
              <button
                data-testid="renderer-openui"
                className={buttonClass}
                onClick={() =>
                  setRendererPreview(
                    rendererPreview === "openui" ? "none" : "openui",
                  )
                }
              >
                OpenUI
              </button>
              <button
                data-testid="renderer-a2ui"
                className={buttonClass}
                onClick={() =>
                  setRendererPreview(
                    rendererPreview === "a2ui" ? "none" : "a2ui",
                  )
                }
              >
                A2UI
              </button>
            </div>
            {rendererPreview === "json-render" && (
              <JsonRenderPanel
                data={{
                  root: "card-1",
                  elements: {
                    "card-1": {
                      type: "Card",
                      props: { title: "Release Status" },
                      children: ["text-1", "text-2"],
                    },
                    "text-1": {
                      type: "Text",
                      props: { content: "All checks passed" },
                      children: [],
                    },
                    "text-2": {
                      type: "Text",
                      props: { content: "Ready for deployment" },
                      children: [],
                    },
                  },
                }}
              />
            )}
            {rendererPreview === "openui" && (
              <OpenUIPanel
                response={[
                  "<Card title='Build Report'>",
                  "  <TextContent>Tests: 42 passed, 0 failed</TextContent>",
                  "  <TextContent>Coverage: 91%</TextContent>",
                  "</Card>",
                ].join("\n")}
              />
            )}
            {rendererPreview === "a2ui" && (
              <A2UIPanel
                messages={[
                  {
                    type: "surface",
                    surfaceId: "preview-1",
                    content: { kind: "text", text: "A2UI surface preview" },
                  },
                ]}
              />
            )}
          </div>
          <p data-testid="backend-tool-prompt" className={subtleHintClass}>
            Optional backend-tool prompt: "What's the weather in San Francisco?" or "Show AAPL latest
            price."
          </p>
        </section>
        <section className={panelCardClass}>
          <div className={sectionLabelClass}>HITL</div>
          <h3 className="text-lg font-semibold text-slate-900">4) HITL (renderAndWaitForResponse)</h3>
          <p data-testid="hitl-prompt" className={subtleHintClass}>
            Prompt: "Ask me to approve clearing todos before executing."
          </p>
          <p data-testid="hitl-status" className="mt-2 text-sm text-slate-700">
            Approval status: {hitlStatus}
          </p>
          <button
            data-testid="open-hitl-preview"
            className={`${buttonClass} mt-2`}
            onClick={() => setShowLocalHitl(true)}
          >
            Open HITL Preview
          </button>
          {showLocalHitl && (
            <div className="mt-3 flex flex-wrap items-center gap-2">
              <ApprovalCard
                onApprove={() => {
                  setHitlStatus("approved");
                  setState({ ...state, todos: [] });
                  setShowLocalHitl(false);
                }}
                onDeny={() => {
                  setHitlStatus("denied");
                  setShowLocalHitl(false);
                }}
              />
            </div>
          )}
        </section>
        <section className={panelCardClass}>
          <div className={sectionLabelClass}>MCP Apps</div>
          <h3 className="text-lg font-semibold text-slate-900">5) MCP App UI (iframe rendering)</h3>
          <p data-testid="mcp-app-prompt" className={subtleHintClass}>
            MCP tools with UI metadata render interactive HTML in a sandboxed iframe.
          </p>
          <div className="mt-3 flex flex-wrap items-center gap-2">
            <button
              data-testid="mcp-app-local-preview-trigger"
              className={buttonClass}
              onClick={() => setShowMcpAppPreview(true)}
            >
              Render MCP App Preview
            </button>
            <button
              data-testid="mcp-app-local-preview-clear"
              className={buttonClass}
              onClick={() => setShowMcpAppPreview(false)}
              disabled={!showMcpAppPreview}
            >
              Clear Preview
            </button>
          </div>
          {showMcpAppPreview && (
            <McpAppFrame
              content='<html><body style="font-family:system-ui;padding:16px"><h2>MCP App Preview</h2><p>This is a sandboxed MCP application rendered via <code>mcp.ui.content</code> metadata.</p><ul><li>Interactive HTML</li><li>Sandboxed iframe</li><li>Auto-resize</li></ul></body></html>'
              mimeType="text/html"
              resourceUri="ui://demo/preview"
            />
          )}
        </section>
      </div>
    </div>
  );

  return (
    <StarterSplitScreen
      rootTestId="home-root"
      pageStyle={pageStyle}
      chatHint="Try: add a todo, set theme color, then ask to clear todos."
      left={leftPane}
      chat={
        <CopilotChat
          className="starter-chat-unified h-full"
          suggestions={V1_RECOMMENDED_SUGGESTIONS}
          labels={{
            title: "awaken assistant",
            initial: "Try: add a todo, set theme color, then ask to clear todos.",
          }}
        />
      }
    />
  );
}

export default function HomePage() {
  const { threadId } = useMainThreadId();
  const agentId = useStarterAgentId();

  return (
    <CopilotKit runtimeUrl="/api/copilotkit" agent={agentId} threadId={threadId}>
      <BaseStarterDemo agentId={agentId} />
    </CopilotKit>
  );
}
