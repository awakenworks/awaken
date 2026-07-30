import { describe, expect, it } from "vitest";
import { suiteHubUrl } from "./suite-navigation";

describe("suite navigation projection", () => {
  /**
   * Cause graph: trusted hub + successful projection -> suite exit; absent hub
   * or projection failure -> standalone shell with no guessed destination.
   *
   * Decision table:
   * | rule | response | request failed | effect |
   * | N1 | exact hub | no | render exact suite exit |
   * | N2 | null | no | render standalone brand |
   * | N3 | any | yes | render standalone brand |
   */
  it("exposes only an exact successfully projected hub", () => {
    expect(suiteHubUrl({ hub_url: "https://cloud.example/products" }, false)).toBe(
      "https://cloud.example/products",
    );
    expect(suiteHubUrl({ hub_url: null }, false)).toBeNull();
    expect(suiteHubUrl({ hub_url: "https://cloud.example/products" }, true)).toBeNull();
  });
});
