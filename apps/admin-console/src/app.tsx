import { type ReactNode, Suspense, lazy } from "react";
import { Navigate, Route, Routes } from "react-router";

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

const AssistantPage = lazy(async () => {
  const module = await import("./pages/assistant-page");
  return { default: module.AssistantPage };
});

export function App() {
  return (
    <Routes>
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
          path="agents/:id"
          element={
            <RouteLoader>
              <AgentEditorPage />
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
          path="assistant"
          element={
            <RouteLoader>
              <AssistantPage />
            </RouteLoader>
          }
        />
      </Route>
      <Route path="*" element={<Navigate to="/" replace />} />
    </Routes>
  );
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
