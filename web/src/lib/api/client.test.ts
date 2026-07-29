import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { getToken } from "./client";

describe("cloud product session bearer", () => {
  // Cause/effect decision table:
  // cloud session bearer -> Authorization header; no bearer -> cookie-only.
  // A localStorage management token is never consulted.
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

  it("uses the short-lived IAM product token", () => {
    globalThis.sessionStorage.setItem("awaken.product.session-bearer", "cloud-token");
    expect(getToken()).toBe("cloud-token");
  });

  it("does not read a management bearer from local storage", () => {
    globalThis.localStorage.setItem("awaken.console.token", "legacy-token");
    expect(getToken()).toBe("");
  });
});
