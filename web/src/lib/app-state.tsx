// App-level presentation state. Workspace is a route/credential scope, not a
// client-authored aggregate: without a server registry the local console exposes
// exactly the configured scope and never invents a parallel roster.

import { createContext, useContext, useEffect, useMemo, useState } from "react";
import type { ReactNode } from "react";
import { workspaceFromPath } from "./api/client";

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

export function AppProvider({ children }: { children: ReactNode }) {
  const [workspaceId, setWorkspaceIdState] = useState(
    () => workspaceFromPath(globalThis.location?.pathname ?? "") || DEFAULT_WS,
  );
  const [identityPresentation, setIdentityPresentation] = useState({
    workspaceName: "",
    organizationName: "",
    userName: "",
  });
  const [locale, setLocale] = useState<Locale>(() => {
    try { return localStorage.getItem("awaken.console.locale") === "zh" ? "zh" : "en"; }
    catch { return "en"; }
  });
  useEffect(() => {
    document.documentElement.setAttribute("data-locale", locale);
    document.documentElement.lang = locale === "zh" ? "zh-Hans" : "en";
    try { localStorage.setItem("awaken.console.locale", locale); }
    catch { /* The current document still supports language changes. */ }
  }, [locale]);

  const value: AppState = useMemo(
    () => ({
      workspaceId,
      workspaceName: identityPresentation.workspaceName || workspaceLabel(workspaceId),
      organizationName: identityPresentation.organizationName || "Local Organization",
      userName: identityPresentation.userName || "Local operator",
      setWorkspaceId: setWorkspaceIdState,
      setIdentityPresentation: (next) => setIdentityPresentation((current) => ({ ...current, ...next })),
      locale,
      toggleLocale: () => setLocale(locale === "en" ? "zh" : "en"),
      t: (en, zh) => (locale === "zh" ? zh : en),
    }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [workspaceId, locale, identityPresentation],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
