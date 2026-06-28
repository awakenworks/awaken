import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { clearAdminToken, readAdminToken, writeAdminToken } from "@/lib/admin-token";
import { setUnauthorizedHandler } from "@/lib/auth-interceptor";
import { configApi, ConfigApiError } from "@/lib/config-api";
import {
  authApi,
  extractAccessTokenFromFragment,
  extractOAuthErrorFromFragment,
} from "@/lib/api/auth";
import { AdminTokenModal } from "./admin-token-modal";
import { useToast } from "./toast-provider";

export type AuthStatus = "checking" | "ok" | "unauthorized" | "missing" | "disconnected";

interface AuthContextValue {
  token: string | null;
  status: AuthStatus;
  oauthEnabled: boolean;
  openTokenModal: () => void;
  logout: () => Promise<void>;
  refresh: () => Promise<void>;
}

const AuthContext = createContext<AuthContextValue | null>(null);

interface PendingPrompt {
  reason: "manual" | "unauthorized";
  resolve: (token: string | null) => void;
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const [token, setToken] = useState<string | null>(() => readAdminToken());
  const [status, setStatus] = useState<AuthStatus>("checking");
  const [pending, setPending] = useState<PendingPrompt | null>(null);
  const [oauthEnabled, setOauthEnabled] = useState(false);
  const toast = useToast();
  const refreshSeqRef = useRef(0);

  const probe = useCallback(async () => {
    const seq = ++refreshSeqRef.current;
    setStatus("checking");
    try {
      const result = await configApi.capabilities();
      if (refreshSeqRef.current !== seq) return;
      setStatus(result.kind === "registry_unavailable" ? "disconnected" : "ok");
    } catch (probeError) {
      if (refreshSeqRef.current !== seq) return;
      if (probeError instanceof ConfigApiError) {
        if (probeError.status === 401) {
          setStatus(readAdminToken() ? "unauthorized" : "missing");
        } else {
          setStatus("disconnected");
        }
      } else {
        setStatus("disconnected");
      }
    }
  }, []);

  const didFragmentCheckRef = useRef(false);
  useEffect(() => {
    if (didFragmentCheckRef.current) return;
    didFragmentCheckRef.current = true;
    const fragment = window.location.hash;
    const oauthError = extractOAuthErrorFromFragment(fragment);
    if (oauthError) {
      toast.error(`Login failed: ${oauthError}`);
      window.history.replaceState(null, "", window.location.pathname + window.location.search);
      return;
    }
    const accessToken = extractAccessTokenFromFragment(fragment);
    if (accessToken) {
      writeAdminToken(accessToken);
      setToken(accessToken);
      window.history.replaceState(null, "", window.location.pathname + window.location.search);
    }
  }, [toast]);

  const didCapCheckRef = useRef(false);
  useEffect(() => {
    if (didCapCheckRef.current) return;
    didCapCheckRef.current = true;
    authApi
      .capabilities()
      .then((caps) => {
        setOauthEnabled(caps.oauth_enabled);
      })
      .catch(() => {
        setOauthEnabled(false);
      });
    void probe();
  }, [probe]);

  useEffect(() => {
    if (!oauthEnabled) return;
    if (status === "missing" || status === "unauthorized") {
      window.location.href = authApi.loginUrl(window.location.href);
    }
  }, [oauthEnabled, status]);

  const promptForToken = useCallback(
    (reason: "manual" | "unauthorized"): Promise<string | null> => {
      return new Promise((resolve) => {
        setPending({ reason, resolve });
      });
    },
    [],
  );

  useEffect(() => {
    if (oauthEnabled) {
      const dispose = setUnauthorizedHandler(async () => null);
      return dispose;
    }
    const dispose = setUnauthorizedHandler(async () => {
      const result = await promptForToken("unauthorized");
      return result;
    });
    return dispose;
  }, [oauthEnabled, promptForToken]);

  const logout = useCallback(async () => {
    try {
      await authApi.logout();
    } catch {
      // best-effort
    }
    clearAdminToken();
    setToken(null);
    setStatus("missing");
    if (oauthEnabled) {
      window.location.href = authApi.loginUrl();
    }
  }, [oauthEnabled]);

  const handleSubmit = useCallback(
    (next: string) => {
      const trimmed = next.trim();
      if (trimmed.length === 0) return;
      writeAdminToken(trimmed);
      setToken(trimmed);
      const current = pending;
      setPending(null);
      current?.resolve(trimmed);
      toast.success("Admin token saved");
      void probe();
    },
    [pending, probe, toast],
  );

  const handleClear = useCallback(() => {
    clearAdminToken();
    setToken(null);
    const current = pending;
    setPending(null);
    current?.resolve(null);
    toast.info("Admin token cleared");
    setStatus("missing");
  }, [pending, toast]);

  const handleCancel = useCallback(() => {
    const current = pending;
    setPending(null);
    current?.resolve(null);
  }, [pending]);

  const openTokenModal = useCallback(() => {
    setPending((current) => (current ? current : { reason: "manual", resolve: () => {} }));
  }, []);

  const value = useMemo<AuthContextValue>(
    () => ({ token, status, oauthEnabled, openTokenModal, logout, refresh: probe }),
    [token, status, oauthEnabled, openTokenModal, logout, probe],
  );

  return (
    <AuthContext.Provider value={value}>
      {children}
      {!oauthEnabled && (
        <AdminTokenModal
          open={pending !== null}
          initialToken={token ?? ""}
          reason={pending?.reason ?? "manual"}
          onSubmit={handleSubmit}
          onClear={handleClear}
          onCancel={handleCancel}
        />
      )}
    </AuthContext.Provider>
  );
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) {
    throw new Error("useAuth must be used inside <AuthProvider>");
  }
  return ctx;
}

export function describeAuthStatus(status: AuthStatus): {
  label: string;
  tone: "ok" | "warn" | "error" | "neutral";
} {
  switch (status) {
    case "ok":
      return { label: "Connected", tone: "ok" };
    case "checking":
      return { label: "Checking…", tone: "neutral" };
    case "missing":
      return { label: "Token missing", tone: "warn" };
    case "unauthorized":
      return { label: "Token rejected", tone: "error" };
    case "disconnected":
      return { label: "Backend unreachable", tone: "error" };
  }
}
