// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, renderHook, waitFor } from "@testing-library/react";

import { ConfigApiError, configResourceApi } from "../../api";
import { createQueryClientWrapper } from "../../../test/query";
import { useConfigMetaQuery } from "./config";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("useConfigMetaQuery", () => {
  it("returns null for optional metadata that is explicitly missing", async () => {
    vi.spyOn(configResourceApi, "getMeta").mockRejectedValue(new ConfigApiError(404, "missing"));

    const { result } = renderHook(
      () => useConfigMetaQuery("agents", "agent-a", { optional: true }),
      {
        wrapper: createQueryClientWrapper(),
      },
    );

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data).toBeNull();
  });

  it("keeps non-404 optional metadata failures in the query error state", async () => {
    const failure = new ConfigApiError(500, "server failed");
    vi.spyOn(configResourceApi, "getMeta").mockRejectedValue(failure);

    const { result } = renderHook(
      () => useConfigMetaQuery("agents", "agent-a", { optional: true }),
      {
        wrapper: createQueryClientWrapper(),
      },
    );

    await waitFor(() => expect(result.current.isError).toBe(true), { timeout: 3_000 });
    expect(result.current.error).toBe(failure);
  });
});
