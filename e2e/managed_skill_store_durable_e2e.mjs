// Durable delivered-skill catalog across a real process restart (ADR-0036).
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
// over the SAME AWAKEN_STORAGE_DIR. `GET /v1/skills` must still list it and a new
// session must still activate it. `awaken-ext-skills` never sees the store — it only
// reads the catalog the host scanned out of it — so this also exercises the
// store-unaware seam end to end.
//
// Deterministic (`skills-durable` mode) so it runs in CI without an API key.
//
// Run: (from e2e/)  node managed_skill_store_durable_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38215);
const BETAS = ['managed-agents-2026-04-01'];
const STORE_DIR = `/tmp/awaken-skillstore-durable-e2e-${process.pid}`;
const MARKER = 'DURABLE_SKILL_MARKER_5501';
const SKILL_MD = `---\ndescription: greet durably\n---\n${MARKER}`;

let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

const listEvents = async (sid) => {
  const evs = [];
  for await (const ev of client.beta.sessions.events.list(sid, { betas: BETAS })) evs.push(ev);
  return evs;
};

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
  await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'discover and use a skill' }] }],
    betas: BETAS,
  });
  const evs = await listEvents(session.id);
  return JSON.stringify(evs.filter((e) => e.type === 'agent.message').map((m) => m.content));
}

async function skillIds() {
  const res = await client.get('/v1/skills');
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
    const a = spawnServer('skills-durable', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR, ...realServerEnv('skills', upstream, { mode: 'skills-durable' }) });
    servers.push(a.server);
    await waitForPort(PORT);

    const created = await client.post('/v1/skills', { body: { id: 'greet', content: SKILL_MD } });
    assert.equal(created.id, 'greet', 'POST /v1/skills stored the skill under its id');
    assert.deepEqual(await skillIds(), ['greet'], 'GET /v1/skills lists the uploaded skill');
    assert.ok((await useSkill()).includes(MARKER), 'the model activated the durable skill (pre-restart)');
    pass('uploaded skill offered, activated, and used within server A');

    // A malformed upload (missing `content`) is rejected, not silently dropped.
    await expectStatus(
      client.post('/v1/skills', { body: { id: 'incomplete' } }),
      400,
      'POST /v1/skills without content',
    );
    pass('POST /v1/skills rejects a body missing `content` with 400');

    // ---- restart: kill A, start B over the SAME storage dir ----
    await stopServer(a.server);
    servers.pop();
    const b = spawnServer('skills-durable', PORT, { AWAKEN_STORAGE_DIR: STORE_DIR, ...realServerEnv('skills', upstream, { mode: 'skills-durable' }) });
    servers.push(b.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });

    assert.deepEqual(await skillIds(), ['greet'], 'GET /v1/skills still lists the skill after restart');
    assert.ok(
      (await useSkill()).includes(MARKER),
      'a new session AFTER restart still activates the durably-configured skill',
    );
    pass('durable skill survived a real process restart and still reaches the model');

    // A server WITHOUT a durable skill store (static `skills` mode) fails a POST
    // closed rather than pretending to persist — the store is required, not optional.
    await stopServer(b.server);
    servers.pop();
    const noStore = spawnServer('skills', PORT, realServerEnv('skills', upstream, { mode: 'skills' }));
    servers.push(noStore.server);
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: `http://127.0.0.1:${PORT}` });
    await expectStatus(
      client.post('/v1/skills', { body: { id: 'greet', content: SKILL_MD } }),
      409,
      'POST /v1/skills on a server with no durable store',
    );
    pass('POST /v1/skills fails closed (409) when the server has no durable skill store');
    console.log('E2E PASS: ADR-0036 delivered-skill catalog is durable across restart.');
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
