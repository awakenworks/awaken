// Durable HITL: under AWAKEN_INGRESS=durable a run parks on a tool needing
// approval, and after the client sends the confirmation the DISPATCH WORKER
// resumes the parked run (not a foreground request). Drives the durable resume
// path — engine::resume_run / resume_into_messages, SharedHost::resume, and the
// worker's resume branch — that the direct-ingress HITL e2e does not reach.
// Deterministic, CI-safe (probe model).

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PORT = 38275;
const STORE = `/tmp/awaken-durable-hitl-${process.pid}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function listEvents(client, id) {
  const e = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) e.push(ev);
  return e;
}
async function until(fn) {
  for (let i = 0; i < 150; i++) {
    const v = await fn();
    if (v) return v;
    await sleep(50);
  }
  return null;
}

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  const srv = spawnServer('probe', PORT, { AWAKEN_INGRESS: 'durable', AWAKEN_STORAGE_DIR: STORE });
  await waitForPort(PORT);
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
  try {
    const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    await client.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'write then read probe.txt' }] }],
      betas: BETAS,
    });

    // The durable run parks on a tool_use awaiting approval.
    const toolUse = await until(async () => (await listEvents(client, s.id)).find((e) => e.type === 'agent.tool_use'));
    assert.ok(toolUse, 'the durable run parked on a tool_use awaiting approval');
    pass('durable run parked on a tool_use (requires_action)');

    // Approve — the DISPATCH WORKER resumes the parked durable run out of band.
    await client.beta.sessions.events.send(s.id, {
      events: [{ type: 'user.tool_confirmation', tool_use_id: toolUse.id, result: 'allow' }],
      betas: BETAS,
    });
    const result = await until(async () => {
      const evs = await listEvents(client, s.id);
      return evs.some((e) => e.type === 'agent.tool_result') ? evs : null;
    });
    assert.ok(result, 'the dispatch worker resumed the parked run and ran the approved tool');
    pass('durable ingress: a parked run resumed by the worker after approval (resume path)');

    console.log('E2E PASS: durable HITL — the dispatch worker resumes a parked run after approval.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(srv.server);
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

main();
