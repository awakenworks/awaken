export type StarterAction = {
  id: string;
  scenarioId: string;
  title: string;
  capability: string;
  prompt: string;
  agentId?: "default" | "permission" | "stopper";
};

export const RECOMMENDED_ACTIONS: StarterAction[] = [
  {
    id: "demo-weather",
    scenarioId: "backend.weather",
    title: "Weather Check",
    capability: "Backend Tool",
    prompt:
      "Use the get_weather tool with location 'Tokyo' and then summarize the result in one sentence.",
    agentId: "default",
  },
  {
    id: "demo-stock",
    scenarioId: "backend.stock",
    title: "Stock Quote",
    capability: "Backend Tool",
    prompt:
      "Use the get_stock_price tool for symbol 'AAPL' and report the returned price.",
    agentId: "default",
  },
  {
    id: "demo-note",
    scenarioId: "backend.append_note",
    title: "Append Note",
    capability: "Stateful Tool",
    prompt:
      "Use the append_note tool and append exactly this note: starter-demo-note",
    agentId: "default",
  },
  {
    id: "demo-server-info",
    scenarioId: "backend.server_info",
    title: "Server Info",
    capability: "Backend Tool",
    prompt:
      "Use the serverInfo tool and return the server name and timestamp from tool output.",
    agentId: "default",
  },
  {
    id: "demo-progress",
    scenarioId: "backend.progress_demo",
    title: "Progress (Default)",
    capability: "Tool Progress",
    prompt:
      "Use the progress_demo tool with scenario 'default' so I can observe tool progress events.",
    agentId: "default",
  },
  {
    id: "demo-progress-pipeline",
    scenarioId: "backend.progress_demo.pipeline",
    title: "Progress (Data Pipeline)",
    capability: "Tool Progress",
    prompt:
      "Use the progress_demo tool with scenario 'data_pipeline' to simulate an ETL data pipeline.",
    agentId: "default",
  },
  {
    id: "demo-progress-deploy",
    scenarioId: "backend.progress_demo.deploy",
    title: "Progress (Deploy)",
    capability: "Tool Progress",
    prompt:
      "Use the progress_demo tool with scenario 'deploy' to simulate a deployment pipeline.",
    agentId: "default",
  },
  {
    id: "demo-progress-build",
    scenarioId: "backend.progress_demo.build",
    title: "Progress (Slow Build)",
    capability: "Tool Progress",
    prompt:
      "Use the progress_demo tool with scenario 'slow_build' to simulate a long compilation.",
    agentId: "default",
  },
  {
    id: "demo-progress-multi",
    scenarioId: "backend.progress_demo.multi_phase",
    title: "Progress (Multi-Phase)",
    capability: "Tool Progress",
    prompt:
      "Use the progress_demo tool with scenario 'multi_phase' to simulate a multi-phase workflow.",
    agentId: "default",
  },
  {
    id: "demo-failing",
    scenarioId: "backend.failing_tool",
    title: "Failure Path",
    capability: "Backend Error",
    prompt:
      "Intentionally call the failingTool to demonstrate backend error handling.",
    agentId: "default",
  },
  {
    id: "demo-ask-user",
    scenarioId: "frontend.ask_user_question",
    title: "Ask User",
    capability: "Frontend Tool",
    prompt:
      "Use the askUserQuestion frontend tool and ask me a short question.",
    agentId: "default",
  },
  {
    id: "demo-bg-color",
    scenarioId: "frontend.set_background_color",
    title: "Background Color",
    capability: "Client-side Action",
    prompt:
      "Use the set_background_color frontend tool (not agent_run) and provide colors ['#dbeafe','#dcfce7'].",
    agentId: "default",
  },
  {
    id: "demo-permission-allow",
    scenarioId: "hitl.permission.approve",
    title: "Permission Approve",
    capability: "Approval Flow",
    prompt:
      "Use the serverInfo tool. I want to test permission approval flow.",
    agentId: "permission",
  },
  {
    id: "demo-permission-deny",
    scenarioId: "hitl.permission.deny",
    title: "Permission Deny",
    capability: "Approval Flow",
    prompt:
      "Use the serverInfo tool. I want to test permission denial flow.",
    agentId: "permission",
  },
];
