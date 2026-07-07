import { createBrowserRouter } from "react-router";
import AppShell from "./components/app/AppShell";
import GatedPage from "./components/app/GatedPage";
import A2aSurface from "./surfaces/a2a";
import AccessSurface from "./surfaces/access";
import CredentialsSurface from "./surfaces/credentials";
import EnvironmentsSurface from "./surfaces/environments";
import HomeSurface from "./surfaces/home";
import McpServersSurface from "./surfaces/mcp-servers";
import ModelsSurface from "./surfaces/models";
import ProjectAgentsSurface from "./surfaces/project-agents";
import ProjectOverviewSurface from "./surfaces/project-overview";
import ProjectSettingsSurface from "./surfaces/project-settings";
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

      // Project scope = the Managed Agents run container.
      { path: "p/:pid/overview", element: <ProjectOverviewSurface /> },
      { path: "p/:pid/sessions", element: <SessionsSurface /> },
      { path: "p/:pid/sessions/:sid", element: <SessionDetailSurface /> },
      { path: "p/:pid/agents", element: <ProjectAgentsSurface /> },
      { path: "p/:pid/environments", element: <EnvironmentsSurface /> },
      { path: "p/:pid/vaults", element: <VaultsSurface /> },
      { path: "p/:pid/memory", element: <GatedPage title="Memory stores" endpoint="/projects/{pid}/v1/memory_stores" /> },
      { path: "p/:pid/deployments", element: <GatedPage title="Deployments" endpoint="/projects/{pid}/v1/deployments" /> },
      { path: "p/:pid/skills", element: <GatedPage title="Skills" endpoint="/projects/{pid}/v1/skills" /> },
      { path: "p/:pid/settings", element: <ProjectSettingsSurface /> },

      // Workspace scope = shared supply + governance.
      { path: "models", element: <ModelsSurface /> },
      { path: "credentials", element: <CredentialsSurface /> },
      { path: "mcp-servers", element: <McpServersSurface /> },
      { path: "a2a-servers", element: <A2aSurface /> },
      { path: "access", element: <AccessSurface /> },
      { path: "settings", element: <SettingsSurface /> },

      { path: "dashboard", element: <GatedPage title="Dashboard" endpoint="/v1/runs/summary · /v1/system/info" /> },
      { path: "audit-log", element: <GatedPage title="Audit log" endpoint="GET /v1/audit-log" /> },
      { path: "datasets", element: <GatedPage title="Datasets" endpoint="/v1/eval/datasets" /> },
      { path: "eval-runs", element: <GatedPage title="Eval runs" endpoint="/v1/eval/runs" /> },
      { path: "*", element: <HomeSurface /> },
    ],
  },
]);
