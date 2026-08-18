// Crash recovery for the durable Memory extraction state machine.
//
// The fake upstream delays every response and counts a request as soon as it
// arrives. The test kills server A after the extractor request arrived but before
// its response, restarts over the same storage directory, rehydrates the original
// Session, and proves the same intent eventually stores exactly one Memory version.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import {
  cleanupFixtureTree,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38243);
const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_HEADERS = { 'anthropic-beta': 'agent-memory-2026-07-22' };
const STORE_DIR = `/tmp/awaken-mem-extract-crash-e2e-${process.pid}`;
const MARKER = 'fact-otter9crash';
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

async function reply(sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events
    .filter((event) => event.type === 'agent.message')
    .map((event) => event.content.map((block) => block.text ?? '').join(''))
    .join('\n');
}

async function turn(sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    betas: BETAS,
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
  });
  return reply(sessionId);
}

async function waitUntil(predicate, message, tries = 80) {
  for (let i = 0; i < tries; i += 1) {
    if (await predicate()) return;
    await sleep(100);
  }
  assert.fail(message);
}

async function hardKill(server) {
  if (server.exitCode !== null || server.signalCode !== null) return;
  const exited = new Promise((resolve) => server.once('exit', resolve));
  server.kill('SIGKILL');
  await exited;
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const servers = [];
  const upstream = await startUpstream('memory', { delayMs: 2_000 });
  try {
    const env = {
      SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR,
      ...realServerEnv('memory', upstream, { mode: 'memory' }),
    };
    const a = spawnServer('memory', PORT, env);
    servers.push(a.server);
    await waitForPort(PORT);

    const store = await client.post('/v1/memory_stores', {
      body: { name: 'crash-extraction' },
      headers: MEMORY_HEADERS,
    });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id, mount_path: '/memory' }],
    });
    assert.ok((await turn(session.id, `remember ${MARKER}`)).includes(MARKER), 'terminal turn committed');

    await waitUntil(
      () => upstream.received >= 2 && upstream.received > upstream.requests.length,
      'extractor inference never became observably in flight',
    );
    await hardKill(a.server);
    servers.pop();
    pass('server A crashed while durable extraction inference was in flight');

    const b = spawnServer('memory', PORT, env);
    servers.push(b.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    // Restart recovery cause/effect table:
    // | durable intent | old extraction lease | Session lifecycle | effect |
    // | Claimed        | live                 | canonical startup supervisor | wait, reclaim, store exactly once |
    // | Claimed        | expired              | canonical startup supervisor | reclaim immediately, store exactly once |
    // | terminal       | any                  | canonical startup supervisor | no duplicate extraction or Memory version |
    // The empty command below only rehydrates the wire projection. Recovery must
    // be owned by the same Coordinator lifecycle supervisor as production, never
    // by pending-tool/history reads or another protocol-specific recovery path.
    await client.beta.sessions.events.send(session.id, { betas: BETAS, events: [] });
    assert.ok((await reply(session.id)).includes(MARKER), 'committed Session rehydrated');
    await waitUntil(async () => {
      const page = await client.get(`/v1/memory_stores/${store.id}/memories?view=full`, {
        headers: MEMORY_HEADERS,
      });
      return JSON.stringify(page).includes(MARKER);
    // Coverage instrumentation can push the durable lease expiry + reclaim
    // cycle beyond the ordinary 14 s window. Keep the wait bounded while
    // preserving the exact-once version assertion below.
    }, 'restarted process did not recover and store the extraction', 300);

    const versions = await client.get(`/v1/memory_stores/${store.id}/memory_versions`, {
      headers: MEMORY_HEADERS,
    });
    assert.equal(versions.data.length, 1, 'recovery commits exactly one logical Memory version');

    const recall = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id, mount_path: '/memory' }],
    });
    assert.ok((await turn(recall.id, 'please recall what you know')).includes(MARKER));
    pass('restart reclaimed the intent exactly once and recall observed the same store');
    console.log('E2E PASS: durable Memory extraction recovers from an in-flight process crash.');
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
