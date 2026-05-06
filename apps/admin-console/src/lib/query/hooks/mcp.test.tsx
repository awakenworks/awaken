// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, renderHook, waitFor } from "@testing-library/react";

import { ConfigApiError, mcpApi, type McpServerStatusResponse } from "../../api";
import { createQueryClientWrapper } from "../../../test/query";
import { useMcpStatusQuery } from "./mcp";

const CONNECTED_STATUS: McpServerStatusResponse = {
  connected: true,
  last_error: null,
  tools: [],
  consecutive_failures: 0,
  last_attempt_at: null,
  last_success_at: null,
  reconnecting: false,
  permanently_failed: false,
};

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("useMcpStatusQuery", () => {
  it("loads MCP status normally", async () => {
    vi.spyOn(mcpApi, "mcpStatus").mockResolvedValue(CONNECTED_STATUS);

    const { result } = renderHook(() => useMcpStatusQuery("server-a"), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data).toEqual(CONNECTED_STATUS);
  });

  it("returns null only for an explicitly missing status endpoint", async () => {
    vi.spyOn(mcpApi, "mcpStatus").mockRejectedValue(new ConfigApiError(404, "missing"));

    const { result } = renderHook(() => useMcpStatusQuery("server-a"), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data).toBeNull();
  });

  it("keeps non-404 status failures in the query error state", async () => {
    const failure = new ConfigApiError(500, "server failed");
    vi.spyOn(mcpApi, "mcpStatus").mockRejectedValue(failure);

    const { result } = renderHook(() => useMcpStatusQuery("server-a"), {
      wrapper: createQueryClientWrapper(),
    });

    await waitFor(() => expect(result.current.isError).toBe(true), { timeout: 3_000 });
    expect(result.current.error).toBe(failure);
  });
});
