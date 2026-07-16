// Independent serde-golden gate for the outbound event wire (fx oracle).
//
// Drives a canonical echo turn and captures the server's *raw* wire JSON straight
// from `GET /v1/sessions/{id}/events` (via fetch, bypassing the SDK's parser — so a
// field the SDK would silently drop still shows up), normalizes it, and diffs it
// against a vendored golden fixture. This is the independent corpus the matrix
// flags as missing: the golden is committed to the repo, decoupled from any live
// SDK round-trip. Regenerate deliberately with UPDATE_GOLDEN=1.
//
// Run: (from e2e/)  node conformance/gate_event_golden_e2e.mjs
//      UPDATE_GOLDEN=1 node conformance/gate_event_golden_e2e.mjs   # re-bless

import assert from 'node:assert/strict';
import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import { withServer } from '../harness.mjs';
import { normalize } from './normalize.mjs';

const BETA = 'managed-agents-2026-04-01';
const PORT = Number(process.env.E2E_PORT ?? 38432);
const HERE = path.dirname(fileURLToPath(import.meta.url));
const GOLDEN = path.join(HERE, '__golden__', 'echo_turn.json');
const H = { 'content-type': 'application/json', 'x-api-key': 'e2e-dummy', 'anthropic-beta': BETA };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const jget = async (u) => (await fetch(u, { headers: H })).json();

async function captureEchoTurn(baseUrl) {
  const created = await (
    await fetch(`${baseUrl}/v1/sessions`, {
      method: 'POST',
      headers: H,
      body: JSON.stringify({ agent: 'assistant', environment_id: 'env_local' }),
    })
  ).json();
  const id = created.id;
  await fetch(`${baseUrl}/v1/sessions/${id}/events`, {
    method: 'POST',
    headers: H,
    body: JSON.stringify({ events: [{ type: 'user.message', content: [{ type: 'text', text: 'work' }] }] }),
  });
  // Settle off 'running' before reading the committed transcript.
  for (let i = 0; i < 30; i++) {
    const s = await jget(`${baseUrl}/v1/sessions/${id}`);
    if (s.status && s.status !== 'running') break;
    await sleep(100);
  }
  const list = await jget(`${baseUrl}/v1/sessions/${id}/events`);
  return normalize(list.data);
}

async function main() {
  try {
    const actual = await withServer('echo', PORT, captureEchoTurn);
    const serialized = JSON.stringify(actual, null, 2) + '\n';

    if (process.env.UPDATE_GOLDEN || !existsSync(GOLDEN)) {
      mkdirSync(path.dirname(GOLDEN), { recursive: true });
      writeFileSync(GOLDEN, serialized);
      console.log(`  wrote golden (${actual.length} events): ${path.relative(process.cwd(), GOLDEN)}`);
      console.log('  event types:', actual.map((e) => e.type).join(', '));
    } else {
      const expected = readFileSync(GOLDEN, 'utf8');
      assert.equal(serialized, expected, 'normalized wire events drifted from the vendored golden');
      console.log('  event types:', actual.map((e) => e.type).join(', '));
      console.log('  matches golden:', path.relative(process.cwd(), GOLDEN));
    }
    console.log('GATE PASS: raw echo-turn wire matches the vendored serde golden.');
    process.exitCode = 0;
  } catch (err) {
    console.error('GATE FAIL:', err.message || err);
    process.exitCode = 1;
  }
}

main();
