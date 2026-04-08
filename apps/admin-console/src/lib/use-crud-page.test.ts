import { afterEach, describe, expect, it, vi } from "vitest";

import { configApi } from "./config-api";
import { loadCrudPageData } from "./use-crud-page";

describe("loadCrudPageData", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("returns list and auxiliary data when both succeed", async () => {
    vi.spyOn(configApi, "list").mockResolvedValue({
      namespace: "models",
      items: [{ id: "model-1" }],
      offset: 0,
      limit: 100,
    });

    await expect(
      loadCrudPageData("models", async () => [["provider-1", "provider-2"]]),
    ).resolves.toEqual({
      items: [{ id: "model-1" }],
      auxiliaryData: [["provider-1", "provider-2"]],
      auxiliaryError: null,
    });
  });

  it("keeps list data when auxiliary loading fails", async () => {
    vi.spyOn(configApi, "list").mockResolvedValue({
      namespace: "providers",
      items: [{ id: "provider-1" }],
      offset: 0,
      limit: 100,
    });

    await expect(
      loadCrudPageData("providers", async () => {
        throw new Error("capabilities unavailable");
      }),
    ).resolves.toEqual({
      items: [{ id: "provider-1" }],
      auxiliaryData: [],
      auxiliaryError: "capabilities unavailable",
    });
  });

  it("still fails the load when the primary list request fails", async () => {
    vi.spyOn(configApi, "list").mockRejectedValue(new Error("list failed"));

    await expect(
      loadCrudPageData("providers", async () => [["openai"]]),
    ).rejects.toThrow("list failed");
  });
});
