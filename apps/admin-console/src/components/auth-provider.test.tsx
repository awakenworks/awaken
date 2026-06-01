// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { StrictMode } from "react";
import { act, cleanup, render, waitFor } from "@testing-library/react";
import { AuthProvider, useAuth } from "./auth-provider";
import { ToastProvider } from "./toast-provider";
import { configApi, ConfigApiError, ADMIN_TOKEN_STORAGE_KEY } from "@/lib/config-api";
import { __resetAuthInterceptorForTesting, hasUnauthorizedHandler } from "@/lib/auth-interceptor";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  __resetAuthInterceptorForTesting();
  localStorage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
});

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
    const spy = vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    renderInStrictMode();

    await waitFor(() => {
      expect(spy).toHaveBeenCalledTimes(1);
    });
  });

  it("manual refresh() still triggers a fresh probe (guard is mount-only)", async () => {
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
    vi.spyOn(configApi, "capabilities").mockResolvedValue(emptyCapabilities());

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("ok");
    });
  });

  it('probe → "unauthorized" on 401 when a token is stored', async () => {
    localStorage.setItem(ADMIN_TOKEN_STORAGE_KEY, "my-token");
    vi.spyOn(configApi, "capabilities").mockRejectedValue(new ConfigApiError(401, "Unauthorized"));

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("unauthorized");
    });
  });

  it('probe → "missing" on 401 when no token is stored', async () => {
    localStorage.removeItem(ADMIN_TOKEN_STORAGE_KEY);
    vi.spyOn(configApi, "capabilities").mockRejectedValue(new ConfigApiError(401, "Unauthorized"));

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("missing");
    });
  });

  it('probe → "disconnected" on a generic (non-ConfigApiError) network error', async () => {
    vi.spyOn(configApi, "capabilities").mockRejectedValue(new Error("fetch failed"));

    const { getCapture } = renderWithCapture();

    await waitFor(() => {
      expect(getCapture().status).toBe("disconnected");
    });
  });

  it("in-flight probe is superseded: second call's outcome wins", async () => {
    // resolve1 controls the first probe; reject2 controls the second.
    let resolve1!: (v: Awaited<ReturnType<typeof configApi.capabilities>>) => void;

    const p1 = new Promise<Awaited<ReturnType<typeof configApi.capabilities>>>((res) => {
      resolve1 = res;
    });
    // Second probe rejects so the final status is "disconnected" (distinct from "ok").
    let reject2!: (e: unknown) => void;
    const p2 = new Promise<Awaited<ReturnType<typeof configApi.capabilities>>>((_, rej) => {
      reject2 = rej;
    });

    const spy = vi.spyOn(configApi, "capabilities").mockReturnValueOnce(p1).mockReturnValueOnce(p2);

    const { getCapture } = renderWithCapture();

    // Wait until the initial probe is in-flight (spy called once).
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(1));

    // Kick off a second probe (rapid refresh) before resolving the first.
    void act(() => {
      void getCapture().refresh();
    });

    await waitFor(() => expect(spy).toHaveBeenCalledTimes(2));

    // Resolve the first probe with "ok" — should be ignored because seq is stale.
    await act(async () => {
      resolve1(emptyCapabilities());
      await p1;
    });

    // The status should NOT be "ok" yet; the second probe is still pending.
    expect(getCapture().status).not.toBe("ok");

    // Resolve the second probe with a network error → "disconnected".
    await act(async () => {
      reject2(new Error("network error"));
      await p2.catch(() => {});
    });

    await waitFor(() => {
      expect(getCapture().status).toBe("disconnected");
    });
  });

  it("unauthorized handler is registered on mount and removed on unmount", async () => {
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
