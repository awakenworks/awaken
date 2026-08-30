import { describe, expect, it } from "vitest";
import type { FileArtifact } from "../../lib/api/types";
import { fileTreeRows, filesForPurpose } from "./FileCatalog";

describe("FileCatalog folder projection", () => {
  it("treats slash-separated logical paths as virtual directories", () => {
    const rows = fileTreeRows([
      { id: "file_1", filename: "reports/2026/report.md", scope: { type: "session", id: "sesn_1" } },
    ] as FileArtifact[]);
    expect(rows.filter((row) => row.directory).map((row) => row.path)).toEqual(["reports", "reports/2026"]);
    expect(rows.find((row) => row.file)?.file?.id).toBe("file_1");
  });

  it("separates reusable inputs from Session-scoped artifacts using the official scope", () => {
    const files = [
      { id: "file_input", filename: "evidence.csv", scope: null },
      { id: "file_output", filename: "reports/brief.md", scope: { type: "session", id: "sesn_1" } },
    ] as FileArtifact[];
    expect(filesForPurpose(files, "input").map((file) => file.id)).toEqual(["file_input"]);
    expect(filesForPurpose(files, "artifact").map((file) => file.id)).toEqual(["file_output"]);
  });
});
