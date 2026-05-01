import { afterEach, describe, expect, it, vi } from "vitest";
import {
  __resetAuthInterceptorForTesting,
  hasUnauthorizedHandler,
  requestUnauthorizedRetry,
  setUnauthorizedHandler,
} from "./auth-interceptor";

afterEach(() => {
  __resetAuthInterceptorForTesting();
});

describe("setUnauthorizedHandler", () => {
  it("registers a handler and reports installation status", () => {
    expect(hasUnauthorizedHandler()).toBe(false);
    setUnauthorizedHandler(async () => "token");
    expect(hasUnauthorizedHandler()).toBe(true);
  });

  it("disposer only removes the active handler", () => {
    const handlerA = vi.fn(async () => "a");
    const handlerB = vi.fn(async () => "b");
    const disposeA = setUnauthorizedHandler(handlerA);
    setUnauthorizedHandler(handlerB);
    disposeA();
    expect(hasUnauthorizedHandler()).toBe(true);
  });
});

describe("requestUnauthorizedRetry", () => {
  it("returns null when no handler is installed", async () => {
    await expect(requestUnauthorizedRetry()).resolves.toBeNull();
  });

  it("invokes the handler exactly once per concurrent burst", async () => {
    let resolveHandler: (value: string | null) => void = () => {};
    const handler = vi.fn(
      () =>
        new Promise<string | null>((resolve) => {
          resolveHandler = resolve;
        }),
    );
    setUnauthorizedHandler(handler);

    const first = requestUnauthorizedRetry();
    const second = requestUnauthorizedRetry();
    const third = requestUnauthorizedRetry();

    resolveHandler("token-1");
    await expect(Promise.all([first, second, third])).resolves.toEqual([
      "token-1",
      "token-1",
      "token-1",
    ]);
    expect(handler).toHaveBeenCalledTimes(1);
  });

  it("re-invokes the handler on a new burst after the previous resolves", async () => {
    const handler = vi
      .fn<UnauthorizedHandlerLike>()
      .mockResolvedValueOnce("token-1")
      .mockResolvedValueOnce(null);
    setUnauthorizedHandler(handler);

    await expect(requestUnauthorizedRetry()).resolves.toBe("token-1");
    await expect(requestUnauthorizedRetry()).resolves.toBeNull();
    expect(handler).toHaveBeenCalledTimes(2);
  });

  it("treats handler rejection as a refusal", async () => {
    const handler = vi.fn(async () => {
      throw new Error("user closed modal");
    });
    setUnauthorizedHandler(handler);
    await expect(requestUnauthorizedRetry()).resolves.toBeNull();
  });
});

type UnauthorizedHandlerLike = () => Promise<string | null>;
