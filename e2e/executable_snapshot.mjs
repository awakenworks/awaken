import fs from 'node:fs';
import path from 'node:path';

// One portable executable-snapshot implementation for every E2E orchestrator.
// Copying is deliberate: a snapshot must not share mutable inode contents with
// a concurrent rebuild, and unlike hard-linking it is valid across filesystems.
export function snapshotExecutable(source, destination) {
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  fs.copyFileSync(source, destination);
  fs.chmodSync(destination, 0o755);
  return destination;
}
