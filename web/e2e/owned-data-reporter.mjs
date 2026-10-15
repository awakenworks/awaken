import fs from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

// Playwright completes webServer teardown before onEnd. Global teardown runs
// earlier, while SQLite still holds Windows file handles in the backend.
export default class OwnedDataReporter {
  constructor({ directory } = {}) {
    this.directory = directory;
  }

  onEnd(result) {
    try {
      const owned = this.directory;
      const prefix = join(tmpdir(), "awaken-console-e2e.");
      if (!owned || dirname(owned) !== tmpdir() || !owned.startsWith(prefix) || owned.length === prefix.length) {
        throw new Error("refusing to clean an unowned browser E2E data directory");
      }
      if (fs.lstatSync(owned, { throwIfNoEntry: false })?.isSymbolicLink()) {
        throw new Error("refusing to clean a linked browser E2E data directory");
      }
      fs.rmSync(owned, { recursive: true, force: true });
    } catch (error) {
      // Reporter exceptions are otherwise swallowed by Playwright. Cleanup
      // failure must reject a passing run and preserve any earlier failure.
      console.error("browser E2E cleanup failed:", error);
      return { status: result.status === "passed" ? "failed" : result.status };
    }
  }
}
