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
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38243);
const BETAS = ['managed-agents-2026-04-01'];
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
      AWAKEN_STORAGE_DIR: STORE_DIR,
      ...realServerEnv('memory', upstream, { mode: 'memory' }),
    };
    const a = spawnServer('memory', PORT, env);
    servers.push(a.server);
    await waitForPort(PORT);

    const store = await client.post('/v1/memory_stores', { body: { name: 'crash-extraction' } });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
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

    // Retrieval rehydrates the exact persisted Session/resource binding. Its
    // reconciler waits for the dead process lease to expire, then resumes the same
    // intent with the same MemoryStore/config and extractor snapshot.
    await client.beta.sessions.events.send(session.id, { betas: BETAS, events: [] });
    assert.ok((await reply(session.id)).includes(MARKER), 'committed Session rehydrated');
    await waitUntil(async () => {
      const page = await client.get(`/v1/memory_stores/${store.id}/memories`);
      return JSON.stringify(page).includes(MARKER);
    }, 'restarted process did not recover and store the extraction', 140);

    const versions = await client.get(`/v1/memory_stores/${store.id}/memory_versions`);
    assert.equal(versions.data.length, 1, 'recovery commits exactly one logical Memory version');

    const recall = await client.beta.sessions.create({
      agent: 'assistant',
      betas: BETAS,
      resources: [{ type: 'memory_store', memory_store_id: store.id, mount_path: '/memory' }],
    });
    assert.ok((await turn(recall.id, 'please recall what you know')).includes(MARKER));
    pass('restart reclaimed the intent exactly once and recall observed the same store');
    console.log('E2E PASS: durable Memory extraction recovers from an in-flight process crash.');
  } finally {
    for (const server of servers) await stopServer(server);
    upstream.close();
    try {
      fs.rmSync(STORE_DIR, { recursive: true, force: true });
    } catch (error) {
      // A SIGKILL can leave a kernel/FUSE mountpoint for the OS to reap after the
      // test process exits. Do not mask the recovery assertion with fixture cleanup.
      if (error?.code !== 'EISDIR' && error?.code !== 'EBUSY') throw error;
    }
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
