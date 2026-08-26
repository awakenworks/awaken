import { rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

export default function globalTeardown(): void {
  const owned = process.env.AWAKEN_E2E_OWNED_DATA_DIR;
  const prefix = join(tmpdir(), "awaken-console-e2e.");
  if (!owned || dirname(owned) !== tmpdir() || !owned.startsWith(prefix)) {
    throw new Error("refusing to clean an unowned browser E2E data directory");
  }
  rmSync(owned, {recursive: true, force: true});
}
