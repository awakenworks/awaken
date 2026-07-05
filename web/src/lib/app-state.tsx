// App-level state: scope (project|workspace), active project, theme, locale.
// All persisted; the project roster comes from the admin plane.

import { useQuery } from "@tanstack/react-query";
import { createContext, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { api } from "./api/client";
import type { Project } from "./api/types";

export type SideScope = "project" | "workspace";
export type Locale = "en" | "zh";

interface AppState {
  scope: SideScope;
  setScope: (s: SideScope) => void;
  projectId: string;
  setProjectId: (id: string) => void;
  projects: Project[];
  projectsReady: boolean;
  theme: string;
  toggleTheme: () => void;
  locale: Locale;
  toggleLocale: () => void;
  t: (en: string, zh: string) => string;
}

const Ctx = createContext<AppState | null>(null);

export function useApp(): AppState {
  const v = useContext(Ctx);
  if (!v) throw new Error("useApp outside provider");
  return v;
}

export function useProjects() {
  return useQuery({
    queryKey: ["projects"],
    queryFn: () => api.get<Project[]>("/v1/config/projects"),
    refetchInterval: 30_000,
  });
}

export function AppProvider({ children }: { children: ReactNode }) {
  const [scope, setScope] = useState<SideScope>(
    () => (localStorage.getItem("awaken.console.scope") as SideScope) || "workspace",
  );
  const [projectId, setProjectId] = useState<string>(
    () => localStorage.getItem("awaken.console.project") ?? "",
  );
  const [theme, setTheme] = useState(() => localStorage.getItem("awaken.console.theme") ?? "light");
  const [locale, setLocale] = useState<Locale>(
    () => (localStorage.getItem("awaken.console.locale") as Locale) || "en",
  );
  const projectsQuery = useProjects();
  const projects = useMemo(() => projectsQuery.data ?? [], [projectsQuery.data]);

  useEffect(() => {
    localStorage.setItem("awaken.console.scope", scope);
  }, [scope]);
  useEffect(() => {
    localStorage.setItem("awaken.console.project", projectId);
  }, [projectId]);
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("awaken.console.theme", theme);
  }, [theme]);
  useEffect(() => {
    document.documentElement.setAttribute("data-locale", locale);
    localStorage.setItem("awaken.console.locale", locale);
  }, [locale]);
  // Roster sentinel: adopt the first project once the roster resolves.
  useEffect(() => {
    if (!projectId && projects.length > 0) setProjectId(projects[0].id);
  }, [projectId, projects]);

  const value: AppState = {
    scope,
    setScope,
    projectId,
    setProjectId,
    projects,
    projectsReady: projectsQuery.isSuccess,
    theme,
    toggleTheme: () => setTheme(theme === "dark" ? "light" : "dark"),
    locale,
    toggleLocale: () => setLocale(locale === "en" ? "zh" : "en"),
    t: (en, zh) => (locale === "zh" ? zh : en),
  };
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
