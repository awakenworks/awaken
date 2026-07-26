// Credential-axis separation under an active embedded-IAM guard: management and
// Managed Agents both accept the workspace service key, while browser protocols
// use separately minted application credentials.
//
// Run: (from e2e/) node management_session_axis_independent_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { deploymentEnv, spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38622);
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';
const BETAS = ['managed-agents-2026-04-01'];

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-session-axis-'));
  const env = deploymentEnv(dir, { identityMode: 'self-managed', controlSealKey: SEAL_KEY });
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

    const noToken = await fetch(`${base}/v1/sessions`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', 'anthropic-beta': BETAS.join(',') },
      body: JSON.stringify({ agent: 'assistant', environment_id: 'env_local' }),
    });
    assert.equal(noToken.status, 401, `session create without a service key is rejected (got ${noToken.status})`);

    const create = await fetch(`${base}/v1/sessions`, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${token}`,
        'content-type': 'application/json',
        'anthropic-beta': BETAS.join(','),
      },
      body: JSON.stringify({ agent: 'assistant', environment_id: 'env_local' }),
    });
    assert.equal(create.status, 200, `service key authorizes session create (got ${create.status})`);
    const session = await create.json();
    assert.ok(session.id && session.type === 'session', `a real session was created: ${JSON.stringify(session).slice(0, 120)}`);
    pass('Managed Agents rejects anonymous access and accepts the workspace service key');

    const get = await fetch(`${base}/v1/sessions/${session.id}`, {
      headers: {
        authorization: `Bearer ${token}`,
        'anthropic-beta': BETAS.join(','),
      },
    });
    assert.equal(get.status, 200, `service key authorizes session read (got ${get.status})`);
    pass('the service credential consistently protects Managed Agents reads and writes');
  } finally {
    await stopServer(server);
    fs.rmSync(dir, { recursive: true, force: true });
  }

  console.log('E2E PASS: service credential protects the Managed Agents axis.');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
