// Managed Environment package behavior across non-container sandbox backends.
// The official Anthropic SDK drives a cloud Environment with package requirements;
// providers that cannot build an immutable package image must reject before an
// agent workload starts. Docker/Podman success + image reuse are exercised by
// managed_container_agent_e2e.mjs with AWAKEN_E2E_PACKAGE_ONLY=1.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { withScenarioServer, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const TMP = path.join(os.tmpdir(), `awaken-environment-packages-e2e-${process.pid}`);

async function exerciseUnsupportedTier(tier, port) {
  const tierRoot = path.join(TMP, tier);
  fs.mkdirSync(tierRoot, { recursive: true });
  await withScenarioServer(
    'environment-matrix',
    'echo',
    port,
    async (base) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
      const environment = await client.beta.environments.create({
        name: `${tier}-packages`,
        config: {
          type: 'cloud',
          packages: { type: 'packages', pip: ['awaken-proof==1'] },
        },
        betas: BETAS,
      });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: environment.id,
        betas: BETAS,
      });
      // Negative decision N1: C1 package requirements plus a backend without
      // immutable package provisioning; E1 SDK send rejects and E2 one durable
      // Session error records the same cause. K1 rejection returns no accepted
      // receipt, so a processed-receipt helper is inapplicable. N1=C1=>E1+E2.
      await assert.rejects(
        client.beta.sessions.events.send(session.id, {
          events: [{
            type: 'user.message',
            content: [{ type: 'text', text: `exercise ${tier} package admission` }],
          }],
          betas: BETAS,
        }),
        /package requirements requested but backend cannot provision packages/,
        `${tier} must fail closed instead of running without declared packages`,
      );
      const events = [];
      for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
        events.push(event);
      }
      assert.ok(
        events.some(
          (event) => event.type === 'session.error'
            && /package requirements requested but backend cannot provision packages/
              .test(event.error?.message ?? event.message ?? JSON.stringify(event)),
        ),
        `${tier} must persist the stable package capability failure: ${JSON.stringify(events)}`,
      );
      pass(`${tier} package requirements fail closed before workload execution`);
    },
    {
      SESSION_ENVIRONMENT_TIER: tier,
      SESSION_DEPLOYMENT_SANDBOX_DIR: path.join(tierRoot, 'sandboxes'),
      SESSION_DEPLOYMENT_STORAGE_DIR: path.join(tierRoot, 'storage'),
    },
  );
}

async function main() {
  fs.rmSync(TMP, { recursive: true, force: true });
  fs.mkdirSync(TMP, { recursive: true });
  try {
    await exerciseUnsupportedTier('local', 38741);
    await exerciseUnsupportedTier('namespace', 38742);
  } finally {
    fs.rmSync(TMP, { recursive: true, force: true });
  }
  console.log('E2E PASS: local and namespace backends reject unsupported Environment packages.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
