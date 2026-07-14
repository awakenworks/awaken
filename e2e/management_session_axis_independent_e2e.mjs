// Session-axis independence under an active embedded-IAM guard (audit #43): the
// management_guard (ADR-0042/0043 P1) wraps ONLY the authoring surfaces
// (/v1/config/*, /v1/vaults/*). The Managed session surface (/v1/sessions) is a
// SEPARATE axis and must NOT be gated by the management bearer token. Existing IAM
// tests only drive /v1/config + /v1/vaults, so the independence was asserted
// nowhere — here we exercise BOTH surfaces with the guard ON.
//
// Run: (from e2e/) node management_session_axis_independent_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38622);
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';
const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-session-axis-'));
  const env = { AWAKEN_MGMT_DIR: dir, AWAKEN_MGMT_SEAL_KEY: SEAL_KEY, AWAKEN_MGMT_IAM: 'embedded' };
  const { server, baseUrl: base } = spawnServer('management', PORT, env);
  try {
    await waitForPort(PORT);
    const token = fs.readFileSync(path.join(dir, 'admin-token'), 'utf8').trim();

    // The guard IS active on the authoring surface: no token -> 401.
    const cfgNoTok = await fetch(`${base}/v1/config/catalog`);
    assert.equal(cfgNoTok.status, 401, `authoring surface is gated (no token -> 401), got ${cfgNoTok.status}`);
    // ...and passes WITH the admin token, proving the guard discriminates by surface.
    const cfgTok = await fetch(`${base}/v1/config/catalog`, { headers: { authorization: `Bearer ${token}` } });
    assert.equal(cfgTok.status, 200, `admin token authorizes the authoring surface, got ${cfgTok.status}`);
    pass('embedded-IAM guard is ACTIVE on /v1/config/* (401 without token, 200 with admin token)');

    // The session axis is INDEPENDENT: creating a session needs NO management token
    // (the management_guard never wraps /v1/sessions). A 401 here would mean the
    // guard leaked onto the session surface.
    const create = await fetch(`${base}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS.join(',') },
      body: JSON.stringify({ agent: 'assistant', environment_id: 'env_local' }),
    });
    assert.equal(create.status, 200, `session create is NOT gated by the management token (got ${create.status})`);
    const session = await create.json();
    assert.ok(session.id && session.type === 'session', `a real session was created: ${JSON.stringify(session).slice(0, 120)}`);
    assert.notEqual(create.status, 401, 'the management IAM guard must not leak onto the session axis');
    pass('embedded-IAM guard does NOT gate /v1/sessions: session created without a management token (axes independent)');

    // Reading it back also needs no management token (same independent axis).
    const get = await fetch(`${base}/v1/sessions/${session.id}`, { headers: { 'anthropic-beta': BETAS.join(',') } });
    assert.equal(get.status, 200, `session read on the independent axis (got ${get.status})`);
    pass('the session axis remains fully usable while the management IAM guard is active');
  } finally {
    await stopServer(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }

  console.log('E2E PASS: session axis is independent of the embedded-IAM management guard.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
