import { createBrowserRouter } from "react-router";
import AppShell from "./components/app/AppShell";
import GatedPage from "./components/app/GatedPage";
import SurfaceRoute from "./components/app/SurfaceRoute";
import A2aSurface from "./surfaces/a2a";
import AccessSurface from "./surfaces/access";
import ArtifactsSurface from "./surfaces/artifacts";
import AssistantSurface from "./surfaces/assistant";
import CredentialsSurface from "./surfaces/credentials";
import DeploymentsSurface from "./surfaces/deployments";
import EnvironmentsSurface from "./surfaces/environments";
import FilesSurface from "./surfaces/files";
import { Navigate } from "react-router";
import MemorySurface from "./surfaces/memory";
import DreamDetailSurface from "./surfaces/dream-detail";
import ModelsSurface from "./surfaces/models";
import McpOverviewSurface from "./surfaces/mcp-overview";
import SkillsSurface from "./surfaces/skills";
import AgentEditorSurface from "./surfaces/agent-editor";
import AgentsSurface from "./surfaces/agents";
import WorkspaceOverviewSurface from "./surfaces/workspace-overview";
import ProtocolsSurface from "./surfaces/protocols";
import SessionDetailSurface from "./surfaces/session-detail";
import SessionsSurface from "./surfaces/sessions";
import SettingsSurface from "./surfaces/settings";
import VaultsSurface from "./surfaces/vaults";

export const router = createBrowserRouter([
  {
    path: "/",
    element: <AppShell />,
    children: [
      { index: true, element: <Navigate to="/w/default/overview" replace /> },

      // A workspace owns BOTH its run resources and its config/supply (ADR-0051):
      // everything is addressed under /w/:ws/… with the active workspace scope.
      { path: "w/:ws/overview", element: <WorkspaceOverviewSurface /> },
      { path: "w/:ws/sessions", element: <SurfaceRoute surface="managed_runtime"><SessionsSurface /></SurfaceRoute> },
      { path: "w/:ws/sessions/:sid", element: <SurfaceRoute surface="managed_runtime"><SessionDetailSurface /></SurfaceRoute> },
      { path: "w/:ws/agents", element: <AgentsSurface /> },
      { path: "w/:ws/agents/:id", element: <AgentEditorSurface /> },
      { path: "w/:ws/assistant", element: <SurfaceRoute surface="managed_runtime"><AssistantSurface /></SurfaceRoute> },
      { path: "w/:ws/environments", element: <SurfaceRoute surface="managed_runtime"><EnvironmentsSurface /></SurfaceRoute> },
      { path: "w/:ws/files", element: <SurfaceRoute surface="managed_runtime"><FilesSurface /></SurfaceRoute> },
      { path: "w/:ws/artifacts", element: <SurfaceRoute surface="managed_runtime"><ArtifactsSurface /></SurfaceRoute> },
      { path: "w/:ws/mcp", element: <SurfaceRoute surface="managed_runtime"><McpOverviewSurface /></SurfaceRoute> },
      { path: "w/:ws/vaults", element: <SurfaceRoute surface="managed_runtime"><VaultsSurface /></SurfaceRoute> },
      { path: "w/:ws/memory", element: <SurfaceRoute surface="managed_runtime"><MemorySurface /></SurfaceRoute> },
      { path: "w/:ws/memory/dreams/:dreamId", element: <SurfaceRoute surface="managed_runtime"><DreamDetailSurface /></SurfaceRoute> },
      { path: "w/:ws/deployments", element: <SurfaceRoute surface="managed_runtime"><DeploymentsSurface /></SurfaceRoute> },
      { path: "w/:ws/skills", element: <SurfaceRoute surface="managed_runtime"><SkillsSurface /></SurfaceRoute> },
      { path: "w/:ws/models", element: <ModelsSurface /> },
      { path: "w/:ws/credentials", element: <CredentialsSurface /> },
      { path: "w/:ws/a2a-servers", element: <SurfaceRoute surface="managed_runtime"><A2aSurface /></SurfaceRoute> },
      { path: "w/:ws/protocols", element: <SurfaceRoute surface="managed_runtime"><ProtocolsSurface /></SurfaceRoute> },
      { path: "w/:ws/access", element: <SurfaceRoute surface="access_management"><AccessSurface /></SurfaceRoute> },
      { path: "w/:ws/settings", element: <SettingsSurface /> },

      { path: "w/:ws/dashboard", element: <GatedPage title="Dashboard" endpoint="/v1/runs/summary" probe="/v1/runs/summary" /> },
      { path: "w/:ws/audit-log", element: <GatedPage title="Audit log" endpoint="/v1/audit-log" probe="/v1/audit-log" /> },
      { path: "w/:ws/datasets", element: <GatedPage title="Datasets" endpoint="/v1/eval/datasets" probe="/v1/eval/datasets" /> },
      { path: "w/:ws/eval-runs", element: <GatedPage title="Eval runs" endpoint="/v1/eval/runs" probe="/v1/eval/runs" /> },
      { path: "*", element: <Navigate to="/w/default/overview" replace /> },
    ],
  },
]);
