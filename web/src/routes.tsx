import { lazy } from "react";
import { createBrowserRouter, Navigate } from "react-router";
import AppShell from "./components/app/AppShell";
import GatedPage from "./components/app/GatedPage";
import SurfaceRoute from "./components/app/SurfaceRoute";

const A2aSurface = lazy(() => import("./surfaces/a2a"));
const AccessSurface = lazy(() => import("./surfaces/access"));
const ArtifactsSurface = lazy(() => import("./surfaces/artifacts"));
const AssistantSurface = lazy(() => import("./surfaces/assistant"));
const CredentialsSurface = lazy(() => import("./surfaces/credentials"));
const DeploymentsSurface = lazy(() => import("./surfaces/deployments"));
const DreamDetailSurface = lazy(() => import("./surfaces/dream-detail"));
const EnvironmentsSurface = lazy(() => import("./surfaces/environments"));
const FilesSurface = lazy(() => import("./surfaces/files"));
const MemorySurface = lazy(() => import("./surfaces/memory"));
const McpOverviewSurface = lazy(() => import("./surfaces/mcp-overview"));
const ModelsSurface = lazy(() => import("./surfaces/models"));
const SkillsSurface = lazy(() => import("./surfaces/skills"));
const AgentEditorSurface = lazy(() => import("./surfaces/agent-editor"));
const AgentsSurface = lazy(() => import("./surfaces/agents"));
const WorkspaceOverviewSurface = lazy(() => import("./surfaces/workspace-overview"));
const ProtocolsSurface = lazy(() => import("./surfaces/protocols"));
const SessionDetailSurface = lazy(() => import("./surfaces/session-detail"));
const SessionsSurface = lazy(() => import("./surfaces/sessions"));
const SettingsSurface = lazy(() => import("./surfaces/settings"));
const VaultsSurface = lazy(() => import("./surfaces/vaults"));
const WebhooksSurface = lazy(() => import("./surfaces/webhooks"));

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
      { path: "w/:ws/webhooks", element: <SurfaceRoute surface="managed_runtime"><WebhooksSurface /></SurfaceRoute> },
      { path: "w/:ws/access", element: <SurfaceRoute surface="access_management"><AccessSurface /></SurfaceRoute> },
      { path: "w/:ws/settings", element: <SettingsSurface /> },

      { path: "w/:ws/dashboard", element: <GatedPage title="Dashboard" endpoint="/v1/runs/summary" fallback="sessions" fallbackLabel="Open Sessions" fallbackLabelZh="打开会话" /> },
      { path: "w/:ws/audit-log", element: <GatedPage title="Audit log" endpoint="/v1/audit-log" fallback="access" fallbackLabel="Open Access" fallbackLabelZh="打开访问控制" /> },
      { path: "w/:ws/datasets", element: <GatedPage title="Datasets" endpoint="/v1/eval/datasets" fallback="agents" fallbackLabel="Open Agents" fallbackLabelZh="打开 Agent" /> },
      { path: "w/:ws/eval-runs", element: <GatedPage title="Eval runs" endpoint="/v1/eval/runs" fallback="agents" fallbackLabel="Open Agents" fallbackLabelZh="打开 Agent" /> },
      { path: "*", element: <Navigate to="/w/default/overview" replace /> },
    ],
  },
]);
