// Stateful Memory qualification over the real Coordinator and fake provider.
// This extends the canonical Session -> extraction -> MemoryStore -> recall path;
// it does not introduce a benchmark-only memory implementation.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38371);
const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const STORE_DIR = path.join(os.tmpdir(), `awaken-memory-conflict-behavior-${process.pid}`);

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

async function turn(sessionId, text) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'exact User receipt anchors the observed Run');
  const { delta } = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta: later }) => later.some((event) => event.type === 'agent.message')
      && later.some((event) => event.type === 'session.status_idle'),
    `memory qualification Run for ${JSON.stringify(text)} to commit`,
  );
  return delta
    .filter((event) => event.type === 'agent.message')
    .map((event) => event.content.map((block) => block.text ?? '').join(''))
    .join('\n');
}

async function memoryHeads(storeId) {
  const page = await client.get(`/v1/memory_stores/${storeId}/memories?view=full`, {
    headers: MEMORY_HEADERS,
  });
  return page.data ?? [];
}

async function memoryVersions(storeId) {
  const page = await client.get(`/v1/memory_stores/${storeId}/memory_versions?view=full`, {
    headers: MEMORY_HEADERS,
  });
  return page.data ?? [];
}

async function main() {
  cleanupFixtureTree(STORE_DIR);
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const upstream = await startUpstream('memory');
  const servers = [];
  try {
    const env = {
      SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
      ...realServerEnv('memory', upstream, { mode: 'memory' }),
    };
    const start = async () => {
      const running = spawnServer('memory', PORT, env);
      servers.push(running.server);
      await waitForPort(PORT);
      client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
      return running.server;
    };
    let server = await start();

    const store = await client.post('/v1/memory_stores', {
      body: { name: 'conflict-and-long-behavior' },
      headers: MEMORY_HEADERS,
    });
    const session = () => client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id, mount_path: '/memory' }],
    });

    // Cause/effect graph and decision table:
    // C1/C2 two terminal Sessions extract the same logical rule path with old/new
    // values; C3 later Sessions bind the same Store; C4 the Coordinator process is
    // replaced over the same durable root. E1 one head contains only the new value;
    // E2 its version log records create then modify; E3 every later behavior uses
    // the new value before and after restart. R1=C1=>old head; R2=C1+C2=>E1+E2;
    // R3=C1+C2+C3=>E3; R4=C1+C2+C3+C4=>E1+E2+E3. Constraint: MemoryStore head and
    // version log remain the sole conflict authority; replies are observations.
    const oldSession = await session();
    await turn(oldSession.id, 'memory-rule-format=PLAIN');
    const oldHead = await waitForValue(
      async () => (await memoryHeads(store.id)).find((head) => head.content?.includes('memory-rule-format=')),
      (head) => head?.content === 'memory-rule-format=PLAIN',
      'first extraction publishes the old rule head',
      { timeoutMs: 15_000, pollMs: 200 },
    );

    const correctionSession = await session();
    await turn(correctionSession.id, 'memory-rule-format=TABLE');
    const correctedHead = await waitForValue(
      async () => (await memoryHeads(store.id)).find((head) => head.path === oldHead.path),
      (head) => head?.content === 'memory-rule-format=TABLE',
      'conflicting extraction replaces the logical rule head',
      { timeoutMs: 15_000, pollMs: 200 },
    );
    assert.equal(correctedHead.id, oldHead.id, 'conflict updates one logical Memory identity');
    const ruleVersions = (await memoryVersions(store.id))
      .filter((version) => version.path === correctedHead.path);
    assert.deepEqual(
      ruleVersions.map((version) => version.operation),
      ['created', 'modified'],
      'the canonical version log retains the superseded fact and one correction',
    );
    assert.deepEqual(
      ruleVersions.map((version) => version.content),
      ['memory-rule-format=PLAIN', 'memory-rule-format=TABLE'],
      'full version projection explains the conflict resolution',
    );
    pass('same-path conflict resolved to one head with auditable versions');

    for (let attempt = 0; attempt < 3; attempt += 1) {
      const reader = await session();
      assert.equal(await turn(reader.id, 'apply-memory-rule'), 'behavior:TABLE');
    }
    pass('three later Sessions consistently apply the corrected behavior');

    await stopServer(server);
    servers.pop();
    server = await start();
    const afterRestart = await session();
    assert.equal(await turn(afterRestart.id, 'apply-memory-rule'), 'behavior:TABLE');
    const durableHead = (await memoryHeads(store.id)).find((head) => head.id === correctedHead.id);
    assert.equal(durableHead?.content, 'memory-rule-format=TABLE');
    assert.equal(
      (await memoryVersions(store.id)).filter((version) => version.path === correctedHead.path).length,
      2,
      'process replacement neither loses nor duplicates the conflict history',
    );
    pass('corrected behavior and two-version history survive process replacement');
    console.log('E2E PASS: Memory conflict resolution and long-lived behavior are durable.');
  } finally {
    for (const server of servers) await stopServer(server);
    upstream.close();
    cleanupFixtureTree(STORE_DIR);
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
