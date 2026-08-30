import { describe, expect, it } from "vitest";
import { sessionResourceAccess } from "./SessionFiles";

describe("Session input access presentation", () => {
  it("makes the protocol-defined read-only File boundary visible", () => {
    expect(sessionResourceAccess({ type: "file" })).toBe("read_only");
  });

  it("preserves explicit Memory access", () => {
    expect(sessionResourceAccess({ type: "memory_store", access: "read_write" })).toBe("read_write");
  });

  it("does not invent an access mode for resource variants that omit one", () => {
    expect(sessionResourceAccess({ type: "github_repository" })).toBeUndefined();
  });
});
