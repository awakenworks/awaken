// Boot invariant (audit #32): a durable management plane MUST NOT start under an
// ephemeral/absent sealing key — sealing vault secrets under a key that changes on
// restart would brick every subsequent boot. So when AWAKEN_MGMT_DIR is set but
// AWAKEN_MGMT_SEAL_KEY is not, `mgmt_seal_key_from_env` panics at boot: the process
// exits non-zero and never binds its port. This pins that fail-closed startup — no
// other test asserts it (they all set the key).
//
// Run: (from e2e/) node management_seal_key_required_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38621);

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-sealkey-required-'));
  // Guard against a leaked key in the ambient env (would defeat the test).
  delete process.env.AWAKEN_MGMT_SEAL_KEY;
  // AWAKEN_MGMT_DIR set, SEAL_KEY absent, IAM off — the durable-store boot path.
  const { server } = spawnServer('management', PORT, { AWAKEN_MGMT_DIR: dir });
  try {
    // Race: the process must EXIT (boot panic) rather than start LISTENING.
    const exited = new Promise((resolve) => server.on('exit', (code, signal) => resolve({ code, signal })));
    const listened = waitForPort(PORT, 12_000).then(() => ({ listened: true })).catch(() => ({ noListen: true }));
    const outcome = await Promise.race([exited, listened]);

    assert.ok(outcome.listened !== true, 'the server must NOT start listening without a sealing key');
    // It exited: a Rust panic aborts non-zero (typically 101) or via SIGABRT.
    const exit = outcome.code ?? (await exited).code;
    const signal = outcome.signal ?? null;
    assert.ok(
      (exit !== null && exit !== 0) || signal !== null,
      `boot must fail non-zero when AWAKEN_MGMT_SEAL_KEY is missing (code=${exit}, signal=${signal})`,
    );
    pass(`durable management plane refuses to boot without AWAKEN_MGMT_SEAL_KEY (exit code=${exit} signal=${signal})`);
  } finally {
    await stopServer(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }

  console.log('E2E PASS: management plane fails closed at boot when AWAKEN_MGMT_SEAL_KEY is absent.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
