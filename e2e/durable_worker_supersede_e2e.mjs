// Supersede stale input — the DROP guarantee, mirroring the dispatch worker's
// stale-input drop (awaken-run-ingress `worker.rs`: input for a superseded ticket
// is never delivered, and a superseded dispatch is never woken again).
//
// The sibling `managed_supersede_e2e.mjs` proves newest-wins: a parked run is
// marked superseded and the new run drives. This proves the OTHER half — that the
// superseded (stale) run is genuinely DROPPED, not merely re-labelled:
//   * reconcile does NOT reclaim/re-drive the superseded run (it is not runnable),
//   * the superseded dispatch stays `Superseded` over time (never Running/Parked-resumed),
//   * committed truth never gains the stale run's effect (no double-commit).
//
// A durable run (probe model) parks on a tool (ticket T); a superseding submit wins
// over it; then we assert the parked-on-T run is dropped for good.
//
// Run: node e2e/durable_worker_supersede_e2e.mjs

import fs from 'node:fs';
import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass, startUpstream, realServerEnv } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39741);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const STORE = `/tmp/awaken-durable-supersede-${process.pid}`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const post = async (path, body) => {
  const res = await fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  return { status: res.status, body: await res.json().catch(() => ({})) };
};
const get = async (path) => (await fetch(`${BASE}${path}`)).json();

async function main() {
  fs.rmSync(STORE, { recursive: true, force: true });
  fs.mkdirSync(STORE, { recursive: true });
  const upstream = await startUpstream('probe');
  const srv = spawnServer('real', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: STORE,
    ...realServerEnv('probe', upstream),
  });
  await waitForPort(PORT);
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
  try {
    // A first turn parks on a tool confirmation — its dispatch sits `Parked` on
    // ticket T in the thread's queue.
    const session = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
    const T = session.id;
    await client.beta.sessions.events.send(T, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'PARK-ON-TICKET-T' }] }],
      betas: BETAS,
    });
    let events = [];
    for await (const ev of client.beta.sessions.events.list(T, { betas: BETAS })) events.push(ev);
    assert.equal(
      events.find((e) => e.type === 'session.status_idle').stop_reason.type,
      'requires_action',
      'first run parked on ticket T (awaiting a tool confirmation)',
    );
    const parkedRow = (await get(`/v1/durable/threads/${T}/dispatches`)).dispatches.find((d) => d.status === 'Parked');
    assert.ok(parkedRow, 'the parked run has a Parked dispatch row');
    const staleRunId = parkedRow.run_id;
    pass(`first run parked on ticket T (dispatch ${staleRunId} is Parked)`);

    // Nothing superseded yet.
    assert.deepEqual((await get(`/v1/durable/threads/${T}/superseded`)).superseded, [], 'nothing superseded before');

    // A superseding submit: the newest turn wins; the parked-on-T run is superseded.
    const sup = await post(`/v1/durable/threads/${T}/supersede`, { text: 'SUPERSEDE-THE-PARKED-RUN' });
    assert.equal(sup.status, 200, 'supersede accepted');
    assert.ok(Array.isArray(sup.body.superseded) && sup.body.superseded.includes(staleRunId), 'the parked-on-T run was superseded');
    pass(`superseding submit marked the parked run superseded: ${sup.body.superseded.join(', ')}`);

    // The superseded run's dispatch is now `Superseded` in the queue.
    const afterSup = (await get(`/v1/durable/threads/${T}/dispatches`)).dispatches;
    assert.equal(afterSup.find((d) => d.run_id === staleRunId)?.status, 'Superseded', 'stale run is Superseded in the queue');
    pass('stale (parked-on-T) dispatch is now Superseded');

    // DROP GUARANTEE #1: reconcile reclaims RUNNABLE work only; a superseded run is
    // not runnable, so it is never re-driven (mirrors the worker never re-applying
    // stale input for a superseded ticket).
    const rec = await post(`/v1/durable/threads/${T}/reconcile`, undefined);
    assert.equal(rec.status, 200, 'reconcile ok');
    assert.ok(!(rec.body.recovered ?? []).includes(staleRunId), 'reconcile does NOT re-drive the superseded run');
    pass('reconcile does not reclaim the superseded run (stale input is dropped, never re-applied)');

    // DROP GUARANTEE #2 + #3: over time the superseded dispatch stays Superseded
    // (never wakes back to Running/Parked-resumed) and committed truth never gains
    // the stale run's effect (no double-commit).
    const msgs1 = (await get(`/v1/durable/threads/${T}/messages`)).messages;
    await sleep(1500);
    const stale = (await get(`/v1/durable/threads/${T}/dispatches`)).dispatches.find((d) => d.run_id === staleRunId);
    assert.equal(stale?.status, 'Superseded', 'the superseded run stayed Superseded (never woken again)');
    const msgs2 = (await get(`/v1/durable/threads/${T}/messages`)).messages;
    assert.deepEqual(msgs2, msgs1, 'committed truth stable — the superseded run never double-committed');
    pass('superseded run stayed dropped: never woken, never double-committed');

    console.log('E2E PASS: supersede stale input — the superseded run is dropped for good (no reclaim, no wake, no double-commit).');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await stopServer(srv.server);
    upstream.close();
    fs.rmSync(STORE, { recursive: true, force: true });
  }
}

main();
