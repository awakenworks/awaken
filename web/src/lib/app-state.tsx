// App-level state: the active workspace (the tenancy scope), theme, locale.
// Tenancy is an edge aspect (ADR-0051): the backend has no workspace registry —
// scope is resolved from the API key or the `/v1/workspaces/{ws}/…` path. So the
// workspace roster is client-held (localStorage), seeded with the default scope.
// Org is a single implicit brand level for now (no backend Org resource yet).

import { createContext, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { setWorkspace } from "./api/client";

export type Locale = "en" | "zh";

export interface Workspace {
  id: string;
  display_name?: string;
}

/** The default scope: addresses the flat `/v1/…` surface (DEFAULT_SCOPE). */
export const DEFAULT_WS = "default";

interface AppState {
  /** The active workspace (tenancy scope). `DEFAULT_WS` = the flat default scope. */
  workspaceId: string;
  setWorkspaceId: (id: string) => void;
  workspaces: Workspace[];
  addWorkspace: (id: string) => void;
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

const WS_LIST_KEY = "awaken.console.workspaces";
const WS_ACTIVE_KEY = "awaken.console.workspace-active";

function loadWorkspaces(): Workspace[] {
  try {
    const raw = localStorage.getItem(WS_LIST_KEY);
    const ids: string[] = raw ? JSON.parse(raw) : [];
    const all = [DEFAULT_WS, ...ids.filter((i) => i && i !== DEFAULT_WS)];
    return all.map((id) => ({ id, display_name: id === DEFAULT_WS ? "Default workspace" : id }));
  } catch {
    return [{ id: DEFAULT_WS, display_name: "Default workspace" }];
  }
}

/** Sync the client `ws()` seam to the active workspace: the default scope routes
 * flat (no prefix); any other workspace routes through `/v1/workspaces/{ws}/…`. */
function syncScope(id: string): void {
  setWorkspace(id === DEFAULT_WS ? "" : id);
}

export function AppProvider({ children }: { children: ReactNode }) {
  const [workspaces, setWorkspaces] = useState<Workspace[]>(loadWorkspaces);
  const [workspaceId, setWorkspaceIdState] = useState<string>(
    () => localStorage.getItem(WS_ACTIVE_KEY) ?? DEFAULT_WS,
  );
  // Awaken Agents brand defaults to dark (awaken-theme.js BRANDS.agents.defaultMode).
  const [theme, setTheme] = useState(() => localStorage.getItem("awaken.console.theme") ?? "dark");
  const [locale, setLocale] = useState<Locale>(
    () => (localStorage.getItem("awaken.console.locale") as Locale) || "en",
  );

  // Bind the client scope seam to the active workspace on mount + on change.
  useEffect(() => {
    syncScope(workspaceId);
    localStorage.setItem(WS_ACTIVE_KEY, workspaceId);
  }, [workspaceId]);
  useEffect(() => {
    localStorage.setItem(
      WS_LIST_KEY,
      JSON.stringify(workspaces.map((w) => w.id).filter((i) => i !== DEFAULT_WS)),
    );
  }, [workspaces]);
  useEffect(() => {
    document.documentElement.setAttribute("data-theme", theme);
    localStorage.setItem("awaken.console.theme", theme);
  }, [theme]);
  useEffect(() => {
    document.documentElement.setAttribute("data-locale", locale);
    localStorage.setItem("awaken.console.locale", locale);
  }, [locale]);

  const setWorkspaceId = (id: string) => {
    syncScope(id);
    setWorkspaceIdState(id);
  };
  const addWorkspace = (id: string) => {
    const clean = id.trim();
    if (!clean) return;
    setWorkspaces((ws) => (ws.some((w) => w.id === clean) ? ws : [...ws, { id: clean, display_name: clean }]));
    setWorkspaceId(clean);
  };

  const value: AppState = useMemo(
    () => ({
      workspaceId,
      setWorkspaceId,
      workspaces,
      addWorkspace,
      theme,
      toggleTheme: () => setTheme(theme === "dark" ? "light" : "dark"),
      locale,
      toggleLocale: () => setLocale(locale === "en" ? "zh" : "en"),
      t: (en, zh) => (locale === "zh" ? zh : en),
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [workspaceId, workspaces, theme, locale],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
