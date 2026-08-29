// Durable delivered-skill catalog across a real process restart (ADR-0036).
// Test design: skill_store_survives_process_replacement
//
// Skills offered on a thread must be able to come from a DURABLE store, not just
// static in-process config: a skill uploaded through `/v1/skills` has to still be
// offered — and still reach the model on activation — after the server process dies
// and a fresh one starts over the same storage dir. The existing single-process
// skills e2e (managed_skills_e2e.mjs) proves discover→activate→use within one
// process; this proves the persistence half the static registry never had.
//
// Flow: POST a distinctive `greet` skill to server A, drive discover→activate→use
// and assert the model used its body, then KILL the server and start a fresh one
// over the SAME SESSION_DEPLOYMENT_STORAGE_DIR. `GET /v1/skills` must still list it and a new
// session must still activate it. `awaken-ext-skills` never sees the store — it only
// reads the catalog the host scanned out of it — so this also exercises the
// store-unaware seam end to end.
//
// Deterministic (`skills-durable` mode) so it runs in CI without an API key.
//
// Run: (from e2e/)  node managed_skill_store_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic, { toFile } from '@anthropic-ai/sdk';
import {
  spawnServer,
  stopServer,
  waitForPort,
  pass,
  startUpstream,
  realServerEnv,
  SKILLS_BETA,
  SKILLS_BETAS,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38215);
const BETAS = ['managed-agents-2026-04-01'];
const SKILLS_HEADERS = { 'anthropic-beta': SKILLS_BETA };
const STORE_DIR = `/tmp/awaken-skillstore-durable-e2e-${process.pid}`;
const MARKER = 'DURABLE_SKILL_MARKER_5501';
const SKILL_MD = `---\nname: greet\ndescription: greet durably\n---\n${MARKER}`;

async function createSkill(content = SKILL_MD) {
  return client.beta.skills.create({
    files: [await toFile(Buffer.from(content), 'SKILL.md')],
    betas: SKILLS_BETAS,
  });
}

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

// Drive one session through discover (list_skills) → activate (Skill) → use, and
// return the concatenated assistant text. The deterministic model replies
// `USED-SKILL: <body>`, so the marker appears only when the skill was truly offered
// and activated.
async function useSkill() {
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
    betas: BETAS,
  });
  // C1=exact User receipt; C2=durable Skill marker reply and terminal. E1=C2
  // after C1 proves this process incarnation used the catalog. K: restart
  // durability remains the Skill-store oracle. Decision U1 C1&&!C2=>retry;
  // U2 C1+C2=>return the committed transcript.
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'discover and use a skill' }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', 'U1 exact durable-Skill User Event receipt');
  const { events } = await waitForSessionEventReceipt(
    client,
    session.id,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.message')
      && delta.some((event) => event.type === 'session.status_idle'),
    'U1 durable Skill Run to commit its marker reply',
  );
  return JSON.stringify(events);
}

async function skillIds() {
  const res = await client.get('/v1/skills', { headers: SKILLS_HEADERS });
  return (res?.data ?? []).map((s) => s.id);
}

// Assert an awaited request rejects with a given HTTP status (the SDK throws an
// APIError carrying `.status` on any non-2xx).
async function expectStatus(promise, code, label) {
  try {
    await promise;
    assert.fail(`${label}: expected HTTP ${code}, got success`);
  } catch (e) {
    assert.equal(e.status, code, `${label}: expected HTTP ${code}, got ${e.status ?? e}`);
  }
}

async function main() {
  fs.rmSync(STORE_DIR, { recursive: true, force: true });
  fs.mkdirSync(STORE_DIR, { recursive: true });
  const servers = [];
  const upstream = await startUpstream('skills');
  try {
    // ---- server A: upload a durable skill, then use it ----
    const a = spawnServer('skills-durable', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('skills', upstream, { mode: 'skills-durable' }) });
    servers.push(a.server);
    await waitForPort(PORT);

    const created = await createSkill();
    assert.ok(created.id.startsWith('skill_'), 'the SDK returns the canonical catalog id');
    assert.deepEqual(await skillIds(), [created.id], 'GET /v1/skills lists the uploaded skill');
    assert.match(
      await useSkill(),
      new RegExp(MARKER),
      'the model activated the durable skill (pre-restart)',
    );
    pass('uploaded skill offered, activated, and used within server A');

    // A malformed multipart upload (missing `files`) is rejected, not silently dropped.
    await expectStatus(
      client.beta.skills.create({ betas: SKILLS_BETAS }),
      400,
      'POST /v1/skills without files',
    );
    pass('POST /v1/skills rejects a multipart body missing `files` with 400');

    // ---- restart: kill A, start B over the SAME storage dir ----
    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('skills-durable', PORT, { SESSION_DEPLOYMENT_STORAGE_DIR: STORE_DIR, ...realServerEnv('skills', upstream, { mode: 'skills-durable' }) });
    servers.push(b.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    assert.deepEqual(await skillIds(), [created.id], 'GET /v1/skills still lists the skill after restart');
    assert.match(
      await useSkill(),
      new RegExp(MARKER),
      'a new session AFTER restart still activates the durably-configured skill',
    );
    pass('durable skill survived a real process restart and still reaches the model');

    // Cause/effect graph / decision table for the deployment storage axis:
    // D1 storage dir=set + create -> Skill is usable before restart and retained after it.
    // D2 storage dir=unset + create -> Skill is visible in the current process only.
    // D3 storage dir=unset + restart -> the volatile Skill is absent; no durable effect
    // is claimed. The canonical ResourceComponent remains complete in both modes;
    // only its selected persistence adapter changes.
    await stopServer(b.server);
    servers.pop();
    const noStore = spawnServer(
      'skills-durable',
      PORT,
      realServerEnv('skills', upstream, { mode: 'skills-durable' }),
    );
    servers.push(noStore.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    const volatile = await createSkill(
      `---\nname: volatile-greet\ndescription: volatile greeting\n---\n${MARKER}`,
    );
    assert.ok(volatile.id.startsWith('skill_'));
    assert.deepEqual(await skillIds(), [volatile.id], 'ephemeral Skill is visible before restart');

    await stopServer(noStore.server);
    servers.pop();
    const freshEphemeral = spawnServer(
      'skills-durable',
      PORT,
      realServerEnv('skills', upstream, { mode: 'skills-durable' }),
    );
    servers.push(freshEphemeral.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    assert.deepEqual(await skillIds(), [], 'ephemeral Skill is absent after process restart');
    pass('ephemeral ResourceComponent accepts Skills without claiming restart durability');
    console.log('E2E PASS: ADR-0036 Skill persistence follows the deployment storage axis.');
  } finally {
    for (const s of servers) await stopServer(s);
    upstream.close();
    fs.rmSync(STORE_DIR, { recursive: true, force: true });
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
