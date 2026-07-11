import { createBrowserRouter } from "react-router";
import AppShell from "./components/app/AppShell";
import GatedPage from "./components/app/GatedPage";
import A2aSurface from "./surfaces/a2a";
import AccessSurface from "./surfaces/access";
import CredentialsSurface from "./surfaces/credentials";
import DeploymentsSurface from "./surfaces/deployments";
import EnvironmentsSurface from "./surfaces/environments";
import HomeSurface from "./surfaces/home";
import McpServersSurface from "./surfaces/mcp-servers";
import MemorySurface from "./surfaces/memory";
import ModelsSurface from "./surfaces/models";
import SkillsSurface from "./surfaces/skills";
import AgentEditorSurface from "./surfaces/agent-editor";
import ProjectAgentsSurface from "./surfaces/project-agents";
import ProjectOverviewSurface from "./surfaces/project-overview";
import SessionDetailSurface from "./surfaces/session-detail";
import SessionsSurface from "./surfaces/sessions";
import SettingsSurface from "./surfaces/settings";
import VaultsSurface from "./surfaces/vaults";

export const router = createBrowserRouter([
  {
    path: "/",
    element: <AppShell />,
    children: [
      { index: true, element: <HomeSurface /> },

      // A workspace owns BOTH its run resources and its config/supply (ADR-0051):
      // everything is addressed under /w/:ws/… with the active workspace scope.
      { path: "w/:ws/overview", element: <ProjectOverviewSurface /> },
      { path: "w/:ws/sessions", element: <SessionsSurface /> },
      { path: "w/:ws/sessions/:sid", element: <SessionDetailSurface /> },
      { path: "w/:ws/agents", element: <ProjectAgentsSurface /> },
      { path: "w/:ws/agents/:id", element: <AgentEditorSurface /> },
      { path: "w/:ws/environments", element: <EnvironmentsSurface /> },
      { path: "w/:ws/vaults", element: <VaultsSurface /> },
      { path: "w/:ws/memory", element: <MemorySurface /> },
      { path: "w/:ws/deployments", element: <DeploymentsSurface /> },
      { path: "w/:ws/skills", element: <SkillsSurface /> },
      { path: "w/:ws/models", element: <ModelsSurface /> },
      { path: "w/:ws/credentials", element: <CredentialsSurface /> },
      { path: "w/:ws/mcp-servers", element: <McpServersSurface /> },
      { path: "w/:ws/a2a-servers", element: <A2aSurface /> },
      { path: "w/:ws/access", element: <AccessSurface /> },
      { path: "w/:ws/settings", element: <SettingsSurface /> },

      { path: "w/:ws/dashboard", element: <GatedPage title="Dashboard" endpoint="/v1/runs/summary · /v1/system/info" /> },
      { path: "w/:ws/audit-log", element: <GatedPage title="Audit log" endpoint="GET /v1/audit-log" /> },
      { path: "w/:ws/datasets", element: <GatedPage title="Datasets" endpoint="/v1/eval/datasets" /> },
      { path: "w/:ws/eval-runs", element: <GatedPage title="Eval runs" endpoint="/v1/eval/runs" /> },
      { path: "*", element: <HomeSurface /> },
    ],
  },
]);
