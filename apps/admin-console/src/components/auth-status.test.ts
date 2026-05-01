import { describe, expect, it } from "vitest";
import { describeAuthStatus, type AuthStatus } from "./auth-provider";

describe("describeAuthStatus", () => {
  const cases: Array<{ status: AuthStatus; label: string; tone: string }> = [
    { status: "ok", label: "Connected", tone: "ok" },
    { status: "checking", label: "Checking…", tone: "neutral" },
    { status: "missing", label: "Token missing", tone: "warn" },
    { status: "unauthorized", label: "Token rejected", tone: "error" },
    { status: "disconnected", label: "Backend unreachable", tone: "error" },
  ];

  for (const { status, label, tone } of cases) {
    it(`maps ${status} to label="${label}" tone="${tone}"`, () => {
      expect(describeAuthStatus(status)).toEqual({ label, tone });
    });
  }
});
