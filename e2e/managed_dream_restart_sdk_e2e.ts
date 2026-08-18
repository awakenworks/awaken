// Official TypeScript SDK restart interleaving for durable Dreams.
//
// Cause/effect decision table:
// | Rule | Durable state at process loss | SDK action after restart | Effect |
// | R1 | running Dream + live provider request | cancel | canceled + cleanup |
// | R2 | canceled Dream | process restart | canceled state is restored |
// | R3 | restored canceled Dream | archive twice | archive is durable/idempotent |
// | R4 | archived Dream | second restart | archived state is restored/listable |
// | R5 | already canceled Dream | cancel again | idempotent canceled projection |

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
// @ts-ignore shared E2E harness is intentionally JavaScript.
import { cleanupFixtureTree, deploymentEnv, pass, realServerEnv, spawnServer, startUpstream, stopServer, waitForPort } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'] as const;
const PORT = Number(process.env.E2E_PORT ?? 38_447);
// realServerEnv publishes this exact provider-backed reference into the Dream
// scenario resolver; using a different authoring id must fail readiness.
const MODEL = 'fake-haiku';
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';

const client = (baseURL: string) => new Anthropic({ apiKey: 'e2e-dummy', baseURL });

async function eventually<T>(read: () => Promise<T>, accept: (value: T) => boolean, label: string) {
  for (let attempt = 0; attempt < 400; attempt += 1) {
    const value = await read();
    if (accept(value)) return value;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`timed out waiting for ${label}`);
}

async function apiStatus(action: () => Promise<unknown>) {
  try {
    await action();
    return 200;
  } catch (error) {
    return error instanceof Anthropic.APIError ? error.status : -1;
  }
}

async function main() {
  const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-dream-restart-data-'));
  const sandboxDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-dream-restart-runs-'));
  const deployment = {
    ...deploymentEnv(dataDir, { identityMode: 'no-login', controlSealKey: SEAL_KEY }),
    SESSION_DEPLOYMENT_STORAGE_DIR: sandboxDir,
  };
  const upstream = await startUpstream('dream', { delayMs: 15_000, models: [MODEL] });
  const processEnv = {
    ...deployment,
    ...realServerEnv('dream', upstream, { mode: 'dream' }),
  };
  let server: ReturnType<typeof spawnServer>['server'] | null = null;
  try {
    const lifetimeA = spawnServer('dream', PORT, processEnv);
    server = lifetimeA.server;
    await waitForPort(PORT, 900_000, server);
    let sdk = client(lifetimeA.baseUrl);

    const sourceSession = await sdk.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: [...BETAS],
    });
    assert.equal(sourceSession.status, 'idle');
    const sourceStore = await sdk.beta.memoryStores.create({ name: 'restart source' });
    await sdk.beta.memoryStores.memories.create(sourceStore.id, {
      path: '/MEMORY.md',
      content: '# Durable source\n',
      view: 'full',
    });
    const created = await sdk.beta.dreams.create({
      inputs: [
        { type: 'memory_store', memory_store_id: sourceStore.id },
        { type: 'sessions', session_ids: [sourceSession.id] },
      ],
      model: MODEL,
    });
    assert.equal(created.status, 'pending');
    await eventually(
      () => sdk.beta.dreams.retrieve(created.id),
      (dream) => dream.status === 'running',
      'the first process to enter Dream execution',
    );
    await eventually(
      async () => upstream.received,
      (received) => received > 0,
      'the delayed provider request to arrive',
    );
    const canceled = await sdk.beta.dreams.cancel(created.id);
    assert.equal(canceled.status, 'canceled');
    assert.equal(await apiStatus(() => sdk.beta.dreams.cancel(created.id)), 200);
    assert.equal((await sdk.beta.dreams.retrieve(created.id)).status, 'canceled');
    pass('R1 official SDK cancels genuinely in-flight Dream execution and completes cleanup');
    await stopServer(server);
    server = null;

    const lifetimeB = spawnServer('dream', PORT, processEnv);
    server = lifetimeB.server;
    await waitForPort(PORT, 900_000, server);
    sdk = client(lifetimeB.baseUrl);
    const restored = await sdk.beta.dreams.retrieve(created.id);
    assert.equal(restored.status, 'canceled');
    assert.equal(restored.archived_at, null);
    const archived = await sdk.beta.dreams.archive(created.id);
    assert.equal(archived.status, 'canceled');
    assert.ok(archived.archived_at);
    const archivedAgain = await sdk.beta.dreams.archive(created.id);
    assert.equal(archivedAgain.archived_at, archived.archived_at);
    pass('R2-R3 canceled state survives restart and archive is durable/idempotent');

    await stopServer(server);
    server = null;
    const lifetimeC = spawnServer('dream', PORT, processEnv);
    server = lifetimeC.server;
    await waitForPort(PORT, 900_000, server);
    sdk = client(lifetimeC.baseUrl);
    const restoredArchived = await sdk.beta.dreams.retrieve(created.id);
    assert.equal(restoredArchived.status, 'canceled');
    assert.equal(restoredArchived.archived_at, archived.archived_at);
    const visible = [];
    for await (const dream of sdk.beta.dreams.list({ include_archived: true })) visible.push(dream);
    assert.ok(visible.some((dream) => dream.id === created.id && dream.archived_at));
    pass('R4-R5 archived state survives a second restart and remains listable');
    console.log('E2E PASS: official TS SDK Dream cancel/archive restart interleaving.');
  } finally {
    if (server) await stopServer(server);
    upstream.close();
    fs.rmSync(dataDir, { recursive: true, force: true });
    cleanupFixtureTree(sandboxDir);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
