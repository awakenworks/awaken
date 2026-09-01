import { describe, expect, it } from "vitest";
import {
  hostedAccessFailure,
  hostedBootstrapDecision,
  hostedSessionEntry,
  suiteHubUrl,
  suiteCloudUrl,
  suiteProductEntryUrl,
} from "./suite-navigation";

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
   * Sibling-selection causal table:
   * exact deployment hub + Flow intent -> same hub with one product request;
   * any stale continuation -> removed, because sibling selection requests its
   * authorized default Workspace rather than leaking an Awaken deep link.
   * Cloud remains the sole authority that may turn the request into a launch.
   */
  it("requests a sibling only through the exact suite hub", () => {
    expect(suiteProductEntryUrl(
      "https://cloud.example/entry?continue=https%3A%2F%2Fagents.example%2Fw%2Fsecret",
      "flow",
    )).toBe("https://cloud.example/entry?product=flow");
  });

  it("derives account destinations only from the projected Cloud origin", () => {
    // Causal design: exact projected hub + closed destination enum -> same
    // Cloud origin and exact account path; product continuation/query/fragment
    // cannot leak into Billing, Settings, Products or global logout.
    expect(suiteCloudUrl(
      "https://cloud.example/entry?continue=secret#fragment",
      "/usage-billing",
    )).toBe("https://cloud.example/usage-billing");
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

  it("verifies an exact hosted route before mounting and classifies failures", () => {
    /**
     * Hosted bootstrap decision table:
     * | rule | hub | route | bearer | effect |
     * | B1 | exact | absent | any | Cloud entry (stale bearer is not authority) |
     * | B2 | exact | exact | absent | Cloud entry with continuation |
     * | B3 | exact | exact | present | verify exact Workspace context |
     * | B4 | null | any | any | standalone local-session path |
     * Probe effects: 401 restarts Cloud login, 403 is terminal deny, all other
     * failures remain retryable/unavailable.
     */
    const hosted = { hub_url: "https://cloud.example/entry" };
    expect(hostedBootstrapDecision(
      hosted,
      "stale-token",
      "https://agents.example/",
      "/",
    )).toEqual({
      kind: "redirect",
      url: "https://cloud.example/entry?continue=https%3A%2F%2Fagents.example%2F",
    });
    expect(hostedBootstrapDecision(
      hosted,
      "product-token",
      "https://agents.example/w/awaken%3Atenant/overview",
      "/w/awaken%3Atenant/overview",
    )).toEqual({ kind: "verify" });
    expect(hostedBootstrapDecision(
      { hub_url: null },
      "",
      "http://localhost/",
      "/",
    )).toEqual({ kind: "standalone" });
    expect([401, 403, 503].map(hostedAccessFailure)).toEqual([
      "restart",
      "denied",
      "unavailable",
    ]);
  });
});
