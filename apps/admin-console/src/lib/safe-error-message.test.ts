import { describe, expect, it } from "vitest";

import { safeErrorMessage } from "./safe-error-message";

describe("safeErrorMessage", () => {
  it("redacts credential patterns from Error messages", () => {
    const message = safeErrorMessage(
      new Error("request failed with Authorization: Bearer sk-real-secret-value"),
    );

    expect(message).toContain("Authorization: ***");
    expect(message).not.toContain("sk-real-secret-value");
  });

  it("redacts credential patterns from non-Error values", () => {
    const message = safeErrorMessage("Cookie: session=raw-session-id");

    expect(message).toContain("Cookie: ***");
    expect(message).not.toContain("raw-session-id");
  });
});
