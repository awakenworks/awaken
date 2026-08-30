// App-level presentation state. Workspace is a route/credential scope, not a
// client-authored aggregate: without a server registry the local console exposes
// exactly the configured scope and never invents a parallel roster.

import { createContext, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { setWorkspace } from "./api/client";

export type Locale = "en" | "zh";

/** The default scope: addresses the flat `/v1/…` surface (DEFAULT_SCOPE). */
export const DEFAULT_WS = "default";

/** Human presentation for an opaque Workspace coordinate. Hosted personal
 * scopes must not leak account-length identifiers into product chrome. */
export function workspaceLabel(id: string): string {
  if (!id || id === DEFAULT_WS) return "Default Workspace";
  if (id.startsWith("awaken:personal:")) return "Personal Workspace";
  if (/^(workspace|wrkspc)_local_/i.test(id)) return "Local Workspace";
  const value = id.replace(/^(workspace|wrkspc|ws)[_:-]/i, "");
  if (value.length > 32 || (/^[a-f\d-]+$/i.test(value) && value.length >= 20)) return "Workspace";
  const name = value.split(/[-_:]+/).filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1)).join(" ");
  return /workspace/i.test(name) ? name : `${name || "Default"} Workspace`;
}

interface AppState {
  /** The active workspace (tenancy scope). `DEFAULT_WS` = the flat default scope. */
  workspaceId: string;
  workspaceName: string;
  organizationName: string;
  userName: string;
  setWorkspaceId: (id: string) => void;
  setIdentityPresentation: (value: { workspaceName?: string; organizationName?: string; userName?: string }) => void;
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

/** Sync the client `ws()` seam to the active workspace: the default scope routes
 * flat (no prefix); any other workspace routes through `/v1/workspaces/{ws}/…`. */
function syncScope(id: string): void {
  setWorkspace(id === DEFAULT_WS ? "" : id);
}

export function AppProvider({ children }: { children: ReactNode }) {
  const [workspaceId, setWorkspaceIdState] = useState(
    () => globalThis.location?.pathname.match(/^\/w\/([^/]+)/)?.[1] ?? DEFAULT_WS,
  );
  const [identityPresentation, setIdentityPresentation] = useState({
    workspaceName: "",
    organizationName: "",
    userName: "",
  });
  // Awaken Agents brand defaults to dark (awaken-theme.js BRANDS.agents.defaultMode).
  const [theme, setTheme] = useState(() => localStorage.getItem("awaken.console.theme") ?? "dark");
  const [locale, setLocale] = useState<Locale>(
    () => (localStorage.getItem("awaken.console.locale") as Locale) || "en",
  );

  // Bind the client scope seam to the active workspace on mount + on change.
  useEffect(() => {
    syncScope(workspaceId);
  }, [workspaceId]);
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
  const value: AppState = useMemo(
    () => ({
      workspaceId,
      workspaceName: identityPresentation.workspaceName || workspaceLabel(workspaceId),
      organizationName: identityPresentation.organizationName || "Local Organization",
      userName: identityPresentation.userName || "Local operator",
      setWorkspaceId,
      setIdentityPresentation: (next) => setIdentityPresentation((current) => ({ ...current, ...next })),
      theme,
      toggleTheme: () => setTheme(theme === "dark" ? "light" : "dark"),
      locale,
      toggleLocale: () => setLocale(locale === "en" ? "zh" : "en"),
      t: (en, zh) => (locale === "zh" ? zh : en),
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [workspaceId, theme, locale, identityPresentation],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
