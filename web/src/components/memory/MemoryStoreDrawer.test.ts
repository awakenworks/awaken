import { describe, expect, it } from "vitest";
import type { MemoryEntry } from "../../lib/api/types";
import { memoryTreeRows } from "./MemoryStoreDrawer";

describe("memory path tree", () => {
  it("projects slash-delimited paths as virtual folders without changing memory ids", () => {
    const memories = [
      { id: "mem_1", path: "customers/acme/profile.md" },
      { id: "mem_2", path: "customers/contoso/profile.md" },
    ] as MemoryEntry[];
    const rows = memoryTreeRows(memories);
    expect(rows.filter((row) => row.directory).map((row) => row.path)).toEqual([
      "customers",
      "customers/acme",
      "customers/contoso",
    ]);
    expect(rows.filter((row) => row.memory).map((row) => row.memory?.id)).toEqual(["mem_1", "mem_2"]);
  });
});
