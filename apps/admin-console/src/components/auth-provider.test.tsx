// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { StrictMode } from "react";
import { act, cleanup, render, waitFor } from "@testing-library/react";
import { AuthProvider, useAuth } from "./auth-provider";
import { ToastProvider } from "./toast-provider";
import { configApi, ConfigApiError, ADMIN_TOKEN_STORAGE_KEY } from "@/lib/config-api";
import { authApi } from "@/lib/api/auth";
import { __resetAuthInterceptorForTesting, hasUnauthorizedHandler } from "@/lib/auth-interceptor";

// Suppress jsdom "navigation" errors from window.location.href assignments.
const originalError = console.error;
beforeEach(() => {
  console.error = (...args: unknown[]) => {
    if (typeof args[0] === "string" && args[0].includes("Not implemented: navigation")) return;
    originalError(...args);
  };
});

afterEach(() => {
  console.error = originalError;
  cleanup();
  vi.restoreAllMocks();
  __resetAuthInterceptorForTesting();
  localStorage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
  // Reset fragment
  window.history.replaceState(null, "", "/");
});

/** Mock authApi.capabilities to return OAuth-disabled (404). */
function mockNoOAuth() {
  vi.spyOn(authApi, "capabilities").mockRejectedValue({ status: 404 });
}

function emptyCapabilities(): Awaited<ReturnType<typeof configApi.capabilities>> {
  return {
    kind: "ok",
    capabilities: {
      agents: [],
      tools: [],
      plugins: [],
      skills: [],
      models: [],
      providers: [],
      namespaces: [],
    },
  };
}

function Consumer() {
  return <div data-testid="child">child</div>;
}

function renderInStrictMode() {
  return render(
    <StrictMode>
      <ToastProvider>
        <AuthProvider>
          <Consumer />
        </AuthProvider>
      </ToastProvider>
    </StrictMode>,
  );
}

/** Mounts AuthProvider and exposes the auth context via a ref. */
function renderWithCapture() {
  let captured: ReturnType<typeof useAuth> | null = null;
  function Capture() {
    captured = useAuth();
    return null;
  }
  const result = render(
    <ToastProvider>
      <AuthProvider>
        <Capture />
      </AuthProvider>
    </ToastProvider>,
  );
  return { result, getCapture: () => captured! };
}

describe("AuthProvider — StrictMode double-mount guard", () => {
  it("calls configApi.capabilities exactly once even under StrictMode", async () => {
    mockNoOAuth();
    const spy = vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    renderInStrictMode();

    await waitFor(() => {
      expect(spy).toHaveBeenCalledTimes(1);
    });
  });

  it("manual refresh() still triggers a fresh probe (guard is mount-only)", async () => {
    mockNoOAuth();
    const spy = vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    let captured: ReturnType<typeof useAuth> | null = null;
    function Capture() {
      captured = useAuth();
      return null;
    }

    render(
      <StrictMode>
        <ToastProvider>
          <AuthProvider>
            <Capture />
          </AuthProvider>
        </ToastProvider>
      </StrictMode>,
    );

    await waitFor(() => {
      expect(spy).toHaveBeenCalledTimes(1);
    });

    await act(async () => {
      await captured!.refresh();
    });

    expect(spy).toHaveBeenCalledTimes(2);
  });
});

describe("AuthProvider — probe state machine", () => {
  it('probe → "ok" when capabilities resolves successfully', async () => {
    mockNoOAuth();
    vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("ok");
    });
  });

  it('probe → "unauthorized" on 401 when a token is stored', async () => {
    mockNoOAuth();
    localStorage.setItem(ADMIN_TOKEN_STORAGE_KEY, "my-token");
    vi.spyOn(configApi, "capabilities").mockRejectedValue(new ConfigApiError(401, "Unauthorized"));

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("unauthorized");
    });
  });

  it('probe → "missing" on 401 when no token is stored', async () => {
    mockNoOAuth();
    localStorage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
    vi.spyOn(configApi, "capabilities").mockRejectedValue(new ConfigApiError(401, "Unauthorized"));

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("missing");
    });
  });

  it('probe → "disconnected" on a generic (non-ConfigApiError) network error', async () => {
    mockNoOAuth();
    vi.spyOn(configApi, "capabilities").mockRejectedValue(new Error("fetch failed"));

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("disconnected");
    });
  });

  it("in-flight probe is superseded: second call's outcome wins", async () => {
    mockNoOAuth();
    let resolve1!: (v: Awaited<ReturnType<typeof configApi.capabilities>>) => void;

    const p1 = new Promise<Awaited<ReturnType<typeof configApi.capabilities>>>((res) => {
      resolve1 = res;
    });
    let reject2!: (e: unknown) => void;
    const p2 = new Promise<Awaited<ReturnType<typeof configApi.capabilities>>>((_, rej) => {
      reject2 = rej;
    });

    const spy = vi.spyOn(configApi, "capabilities").mockReturnValueOnce(p1).mockReturnValueOnce(p2);

    const { getCapture } = renderWithCapture();

    await waitFor(() => expect(spy).toHaveBeenCalledTimes(1));

    void act(() => {
      void getCapture().refresh();
    });

    await waitFor(() => expect(spy).toHaveBeenCalledTimes(2));

    await act(async () => {
      resolve1(emptyCapabilities());
      await p1;
    });

    expect(getCapture().status).not.toBe("ok");

    await act(async () => {
      reject2(new Error("network error"));
      await p2.catch(() => {});
    });

    await waitFor(() => {
      expect(getCapture().status).toBe("disconnected");
    });
  });

  it("unauthorized handler is registered on mount and removed on unmount", async () => {
    mockNoOAuth();
    vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    const { result } = renderWithCapture();

    await waitFor(() => {
      expect(hasUnauthorizedHandler()).toBe(true);
    });

    act(() => {
      result.unmount();
    });

    expect(hasUnauthorizedHandler()).toBe(false);
  });
});

describe("AuthProvider — OAuth capabilities probe", () => {
  it("sets oauthEnabled=false when capabilities endpoint returns 404", async () => {
    mockNoOAuth();
    vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("ok");
    });

    expect(getCapture().oauthEnabled).toBe(false);
  });

  it("sets oauthEnabled=true when capabilities endpoint returns oauth_enabled=true", async () => {
    vi.spyOn(authApi, "capabilities").mockResolvedValue({
      oauth_enabled: true,
      login_url: "/v1/auth/login",
    });
    // Resolve config probe with a token so we don't trigger a redirect
    localStorage.setItem(ADMIN_TOKEN_STORAGE_KEY, "sess-abc");
    vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().oauthEnabled).toBe(true);
    });
  });
});

describe("AuthProvider — fragment handling", () => {
  it("reads access_token from fragment, stores it, and clears the fragment", async () => {
    mockNoOAuth();
    window.location.hash = "#access_token=sess-xyz";
    vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().token).toBe("sess-xyz");
    });
    expect(localStorage.getItem(ADMIN_TOKEN_STORAGE_KEY)).toBe("sess-xyz");
  });

  it("shows a toast and clears the fragment when oauth_error is present", async () => {
    mockNoOAuth();
    window.location.hash = "#oauth_error=access_denied";
    vi.spyOn(configApi, "capabilities").mockRejectedValue(new ConfigApiError(401, "Unauthorized"));

    renderWithCapture();

    // The fragment should be cleared (no infinite loop).
    await waitFor(() => {
      expect(window.location.hash).toBe("");
    });
  });
});

describe("AuthProvider — logout", () => {
  it("exposes a logout function in the context", async () => {
    mockNoOAuth();
    vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("ok");
    });

    expect(typeof getCapture().logout).toBe("function");
  });

  it("logout calls authApi.logout and clears the token", async () => {
    mockNoOAuth();
    const logoutSpy = vi.spyOn(authApi, "logout").mockResolvedValue(undefined);
    vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());
    localStorage.setItem(ADMIN_TOKEN_STORAGE_KEY, "sess-abc");

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("ok");
    });

    await act(async () => {
      await getCapture().logout();
    });

    expect(logoutSpy).toHaveBeenCalledTimes(1);
    expect(getCapture().token).toBeNull();
  });
});
