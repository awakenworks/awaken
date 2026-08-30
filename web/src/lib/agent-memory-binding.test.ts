import { describe, expect, it } from "vitest";
import type { InputBinding } from "./api/types";
import { reconcileMemoryBinding, shouldEnableMemoryExtraction } from "./agent-memory-binding";

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

  it("pins the first mounted store and deterministically reselects after removal", () => {
    const first = binding("memory_store");
    const second = { ...binding("memory_store"), binding_id: "memory-2", target: { kind: "memory_store" as const, id: "memory_store-2" } };
    const added = reconcileMemoryBinding([], [first, second], { plugins: [], plugin_config: {} });
    expect(added.plugins).toEqual(["memory"]);
    expect(added.plugin_config?.memory).toMatchObject({ binding_id: first.binding_id });
    const removed = reconcileMemoryBinding([first, second], [second], {
      plugins: ["memory"],
      plugin_config: { memory: { binding_id: first.binding_id } },
    });
    expect(removed.plugin_config?.memory).toMatchObject({ binding_id: second.binding_id });
  });

  it("clears a removed binding instead of letting runtime guess", () => {
    const first = binding("memory_store");
    const patch = reconcileMemoryBinding([first], [], {
      plugins: ["memory"],
      plugin_config: { memory: { binding_id: first.binding_id, recall_enabled: true } },
    });
    expect(patch.plugin_config?.memory).toEqual({ recall_enabled: true });
  });
});
