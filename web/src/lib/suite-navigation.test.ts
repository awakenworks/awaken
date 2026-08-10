import { describe, expect, it } from "vitest";
import { hostedSessionEntry, suiteHubUrl } from "./suite-navigation";

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

  /**
   * Cause graph: hosted projection + absent product bearer -> Cloud hub;
   * standalone projection or an existing bearer -> product console.
   *
   * Decision table:
   * | rule | hub | product bearer | effect |
   * | S1 | exact | absent | redirect to the exact hub with the current deep link |
   * | S2 | exact | present | continue to the hosted product |
   * | S3 | null | absent | continue to standalone local setup |
   * | S4 | null | present | continue; do not reinterpret the credential |
   */
  it("routes only an unauthenticated hosted browser through the suite hub", () => {
    const hosted = { hub_url: "https://cloud.example/products" };
    const standalone = { hub_url: null };
    expect(hostedSessionEntry(hosted, "", "https://agents.example/w/awaken%3Atenant/sessions/session-1?tab=runs#latest")).toEqual({
      kind: "redirect",
      url: "https://cloud.example/products?continue=https%3A%2F%2Fagents.example%2Fw%2Fawaken%253Atenant%2Fsessions%2Fsession-1%3Ftab%3Druns%23latest",
    });
    expect(hostedSessionEntry(hosted, "product-token", "https://agents.example/w/workspace")).toEqual({ kind: "continue" });
    expect(hostedSessionEntry(standalone, "", "http://localhost/w/workspace")).toEqual({ kind: "continue" });
    expect(hostedSessionEntry(standalone, "product-token", "http://localhost/w/workspace")).toEqual({ kind: "continue" });
  });
});
