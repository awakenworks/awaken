import { describe, expect, it } from "vitest";
import type { InputBinding } from "./api/types";
import { shouldEnableMemoryExtraction } from "./agent-memory-binding";

const binding = (kind: "memory_store" | "file"): InputBinding => ({
  binding_id: kind,
  target: { kind, id: `${kind}-1` },
  mount_path: kind === "memory_store" ? "/mnt/memory" : "/mnt/file",
  access: kind === "memory_store" ? "read_write" : "read_only",
});

describe("Memory binding cause/effect graph", () => {
  // New Memory Store + extraction not configured -> enable extraction defaults.
  // Existing Memory Store, non-memory resources, or an already enabled plugin -> no mutation.
  it("enables extraction only on the first Memory Store transition", () => {
    expect(shouldEnableMemoryExtraction([], [binding("memory_store")], [])).toBe(true);
    expect(shouldEnableMemoryExtraction([binding("memory_store")], [binding("memory_store")], [])).toBe(false);
    expect(shouldEnableMemoryExtraction([], [binding("file")], [])).toBe(false);
    expect(shouldEnableMemoryExtraction([], [binding("memory_store")], ["memory"])).toBe(false);
  });
});
