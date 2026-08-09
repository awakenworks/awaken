import { describe, expect, it } from "vitest";
import type { FileArtifact } from "../../lib/api/types";
import { fileTreeRows } from "./FileCatalog";

describe("FileCatalog folder projection", () => {
  it("treats slash-separated logical paths as virtual directories", () => {
    const rows = fileTreeRows([
      { id: "file_1", filename: "report.md", logical_path: "reports/2026/report.md" },
    ] as FileArtifact[]);
    expect(rows.filter((row) => row.directory).map((row) => row.path)).toEqual(["reports", "reports/2026"]);
    expect(rows.find((row) => row.file)?.file?.id).toBe("file_1");
  });
});
