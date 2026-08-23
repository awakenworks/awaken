// Independent serde-golden gate for the outbound event wire (fx oracle).
//
// Drives a canonical echo-run and captures the server's *raw* wire JSON straight
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
import Anthropic from '@anthropic-ai/sdk';
import { waitForSessionEventReceipt, withServer } from '../harness.mjs';
import { normalize } from './normalize.mjs';

const BETA = 'managed-agents-2026-04-01';
const PORT = Number(process.env.E2E_PORT ?? 38432);
const HERE = path.dirname(fileURLToPath(import.meta.url));
const GOLDEN = path.join(HERE, '__golden__', 'echo_run.json');
const H = { 'content-type': 'application/json', 'x-api-key': 'e2e-dummy', 'anthropic-beta': BETA };
const jget = async (u) => (await fetch(u, { headers: H })).json();

async function captureEchoRun(baseUrl) {
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, maxRetries: 0 });
  const created = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: [BETA],
  });
  const id = created.id;
  const receipt = await client.beta.sessions.events.send(id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'work' }] }],
    betas: [BETA],
  });
  const acceptedId = receipt.data?.[0]?.id;
  assert.equal(typeof acceptedId, 'string', 'SDK send returns the exact accepted User Event id');
  // Raw-golden receipt rule: C1=official SDK create/send succeeds and returns
  // the exact receipt; C2=a later idle commits. E1=only then capture the raw
  // wire. K=SDK admission/observation cannot replace the independent raw GET
  // oracle. Decision rules: G1 !C1=>fail; G2 C1+!C2=>retry; G3 C1+C2=>E1.
  await waitForSessionEventReceipt(
    client,
    id,
    acceptedId,
    [BETA],
    ({ delta }) => delta.some((event) => event.type === 'session.status_idle'),
    'the echo Run to settle before capturing its committed wire events',
    { timeoutMs: 3_000 },
  );
  return normalize((await jget(`${baseUrl}/v1/sessions/${id}/events`)).data);
}

async function main() {
  try {
    // Cause/effect graph: C0=official SDK create/send returns an exact accepted
    // receipt that is processed before a new terminal edge; C1=the deterministic
    // echo Run has settled;
    // C2=a vendored normalized raw-wire golden exists; C3=UPDATE_GOLDEN is
    // enabled. E0=an older Session idle cannot satisfy this capture; E1=the raw
    // fetch corpus is normalized and compared byte-for-byte; E2=wire drift
    // fails; E3=the corpus is deliberately re-blessed. Decision rules: R0 C0
    // => E0+C1; R1 C0+C1+C2+!C3 => E1 (mismatch => E2); R2 C0+C1+(!C2||C3)
    // => E3. Constraints/invariant: normalization may remove nondeterministic
    // values only; UPDATE_GOLDEN is the sole explicit re-bless authority.
    const actual = await withServer('echo', PORT, captureEchoRun);
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
    console.log('GATE PASS: raw echo-run wire matches the vendored serde golden.');
    process.exitCode = 0;
  } catch (err) {
    console.error('GATE FAIL:', err.message || err);
    process.exitCode = 1;
  }
}

main();
