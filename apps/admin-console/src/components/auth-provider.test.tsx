// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { StrictMode } from "react";
import { act, cleanup, render, waitFor } from "@testing-library/react";
import { AuthProvider, useAuth } from "./auth-provider";
import { ToastProvider } from "./toast-provider";
import { configApi } from "@/lib/config-api";
import { __resetAuthInterceptorForTesting } from "@/lib/auth-interceptor";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  __resetAuthInterceptorForTesting();
});

function emptyCapabilities(): Awaited<ReturnType<typeof configApi.capabilities>> {
  return {
    agents: [],
    tools: [],
    plugins: [],
    skills: [],
    models: [],
    providers: [],
    namespaces: [],
  } as Awaited<ReturnType<typeof configApi.capabilities>>;
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

describe("AuthProvider — StrictMode double-mount guard", () => {
  it("calls configApi.capabilities exactly once even under StrictMode", async () => {
    const spy = vi
      .spyOn(configApi, "capabilities")
      .mockResolvedValue(emptyCapabilities());

    renderInStrictMode();

    await waitFor(() => {
      expect(spy).toHaveBeenCalledTimes(1);
    });
  });

  it("manual refresh() still triggers a fresh probe (guard is mount-only)", async () => {
    const spy = vi
      .spyOn(configApi, "capabilities")
      .mockResolvedValue(emptyCapabilities());

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
