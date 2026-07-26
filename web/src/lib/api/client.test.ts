import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { getToken, setToken } from "./client";

describe("cloud product session bearer", () => {
  // Cause graph: cloud session -> use cloud token; otherwise manual token ->
  // use manual token; otherwise remain anonymous.  The first case also proves
  // cloud precedence, so this is the minimized decision table.
  beforeAll(() => {
    const storage = () => {
      const values = new Map<string, string>();
      return {
        clear: () => values.clear(),
        getItem: (key: string) => values.get(key) ?? null,
        removeItem: (key: string) => values.delete(key),
        setItem: (key: string, value: string) => values.set(key, value),
      };
    };
    Object.defineProperty(globalThis, "sessionStorage", { value: storage() });
    Object.defineProperty(globalThis, "localStorage", { value: storage() });
  });

  beforeEach(() => {
    globalThis.sessionStorage.clear();
    globalThis.localStorage.clear();
  });

  it("prefers the short-lived IAM product token over a local manual token", () => {
    setToken("local-token");
    globalThis.sessionStorage.setItem("awaken.product.session-bearer", "cloud-token");
    expect(getToken()).toBe("cloud-token");
  });

  it("retains the local manual-token path when no cloud session exists", () => {
    setToken("local-token");
    expect(getToken()).toBe("local-token");
  });

  it("remains anonymous when neither credential source exists", () => {
    expect(getToken()).toBe("");
  });
});
