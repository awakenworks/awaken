import { type ReactNode, Suspense, lazy } from "react";
import {
  Navigate,
  Route,
  Routes,
  createBrowserRouter,
  createRoutesFromElements,
} from "react-router";

const AdminLayout = lazy(async () => {
  const module = await import("./components/admin-layout");
  return { default: module.AdminLayout };
});

const DashboardPage = lazy(async () => {
  const module = await import("./pages/dashboard-page");
  return { default: module.DashboardPage };
});

const AgentsPage = lazy(async () => {
  const module = await import("./pages/agents-page");
  return { default: module.AgentsPage };
});

const SkillsPage = lazy(async () => {
  const module = await import("./pages/skills-page");
  return { default: module.SkillsPage };
});

const AgentEditorPage = lazy(async () => {
  const module = await import("./pages/agent-editor-page");
  return { default: module.AgentEditorPage };
});

const ModelsPage = lazy(async () => {
  const module = await import("./pages/models-page");
  return { default: module.ModelsPage };
});

const ProvidersPage = lazy(async () => {
  const module = await import("./pages/providers-page");
  return { default: module.ProvidersPage };
});

const McpServersPage = lazy(async () => {
  const module = await import("./pages/mcp-servers-page");
  return { default: module.McpServersPage };
});

const McpServerDetailPage = lazy(async () => {
  const module = await import("./pages/mcp-server-detail-page");
  return { default: module.McpServerDetailPage };
});

const SkillDetailPage = lazy(async () => {
  const module = await import("./pages/skill-detail-page");
  return { default: module.SkillDetailPage };
});

const AssistantPage = lazy(async () => {
  const module = await import("./pages/assistant-page");
  return { default: module.AssistantPage };
});

const EvalReportsPage = lazy(async () => {
  const module = await import("./pages/eval-reports-page");
  return { default: module.EvalReportsPage };
});

const AgentDashboardPage = lazy(async () => {
  const module = await import("./pages/agent-dashboard-page");
  return { default: module.AgentDashboardPage };
});

const AuditLogPage = lazy(async () => {
  const module = await import("./pages/audit-log-page");
  return { default: module.AuditLogPage };
});

const ToolsPage = lazy(async () => {
  const module = await import("./pages/tools-page");
  return { default: module.ToolsPage };
});

const ToolEditorPage = lazy(async () => {
  const module = await import("./pages/tool-editor-page");
  return { default: module.ToolEditorPage };
});

const DatasetsPage = lazy(async () => {
  const module = await import("./pages/datasets-page");
  return { default: module.DatasetsPage };
});

const DatasetDetailPage = lazy(async () => {
  const module = await import("./pages/dataset-detail-page");
  return { default: module.DatasetDetailPage };
});

const EvalRunsPage = lazy(async () => {
  const module = await import("./pages/eval-runs-page");
  return { default: module.EvalRunsPage };
});

const EvalRunDetailPage = lazy(async () => {
  const module = await import("./pages/eval-run-detail-page");
  return { default: module.EvalRunDetailPage };
});

/// Routes are declared once and reused via the data router so that v7
/// hooks like `useBlocker` work. `<Routes>` (kept exported for tests
/// that prefer the legacy router) renders the same structure.
export function appRoutes() {
  return (
    <>
      <Route
        path="/"
        element={
          <RouteLoader>
            <AdminLayout />
          </RouteLoader>
        }
      >
        <Route
          index
          element={
            <RouteLoader>
              <DashboardPage />
            </RouteLoader>
          }
        />
        <Route
          path="agents"
          element={
            <RouteLoader>
              <AgentsPage />
            </RouteLoader>
          }
        />
        <Route
          path="skills"
          element={
            <RouteLoader>
              <SkillsPage />
            </RouteLoader>
          }
        />
        <Route
          path="skills/:id"
          element={
            <RouteLoader>
              <SkillDetailPage />
            </RouteLoader>
          }
        />
        <Route
          path="agents/:id"
          element={
            <RouteLoader>
              <AgentEditorPage />
            </RouteLoader>
          }
        />
        <Route
          path="agents/:id/dashboard"
          element={
            <RouteLoader>
              <AgentDashboardPage />
            </RouteLoader>
          }
        />
        <Route
          path="models"
          element={
            <RouteLoader>
              <ModelsPage />
            </RouteLoader>
          }
        />
        <Route
          path="providers"
          element={
            <RouteLoader>
              <ProvidersPage />
            </RouteLoader>
          }
        />
        <Route
          path="mcp-servers"
          element={
            <RouteLoader>
              <McpServersPage />
            </RouteLoader>
          }
        />
        <Route
          path="mcp-servers/:id"
          element={
            <RouteLoader>
              <McpServerDetailPage />
            </RouteLoader>
          }
        />
        <Route
          path="assistant"
          element={
            <RouteLoader>
              <AssistantPage />
            </RouteLoader>
          }
        />
        <Route
          path="datasets"
          element={
            <RouteLoader>
              <DatasetsPage />
            </RouteLoader>
          }
        />
        <Route
          path="datasets/:id"
          element={
            <RouteLoader>
              <DatasetDetailPage />
            </RouteLoader>
          }
        />
        <Route
          path="eval-runs"
          element={
            <RouteLoader>
              <EvalRunsPage />
            </RouteLoader>
          }
        />
        <Route
          path="eval-runs/:id"
          element={
            <RouteLoader>
              <EvalRunDetailPage />
            </RouteLoader>
          }
        />
        <Route
          path="eval-reports"
          element={
            <RouteLoader>
              <EvalReportsPage />
            </RouteLoader>
          }
        />
        <Route
          path="audit-log"
          element={
            <RouteLoader>
              <AuditLogPage />
            </RouteLoader>
          }
        />
        <Route
          path="tools"
          element={
            <RouteLoader>
              <ToolsPage />
            </RouteLoader>
          }
        />
        <Route
          path="tools/:id"
          element={
            <RouteLoader>
              <ToolEditorPage />
            </RouteLoader>
          }
        />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </>
  );
}

export const router = createBrowserRouter(createRoutesFromElements(appRoutes()));

export function App() {
  return <Routes>{appRoutes()}</Routes>;
}

function RouteLoader({ children }: { children: ReactNode }) {
  return (
    <Suspense
      fallback={
        <div className="min-h-screen px-6 py-8 text-sm text-slate-500">
          Loading admin console...
        </div>
      }
    >
      {children}
    </Suspense>
  );
}
