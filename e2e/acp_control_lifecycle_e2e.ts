// Served-process coverage for ACP cooperative pause/resume and continuation
// relaunch failure. Every control enters through HTTP; the only fixture is the
// external newline ACP subprocess composed by awaken-scenario-host.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
  waitForValue,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39772);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];

async function post(route: string, body: unknown): Promise<{ status: number; body: any }> {
  const response = await fetch(`${BASE}${route}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  return { status: response.status, body: await response.json().catch(() => ({})) };
}

async function createAcpSession(client: Anthropic): Promise<any> {
  return client.beta.sessions.create({
    agent: 'acp-agent',
    environment_id: 'env_local',
    betas: BETAS,
  });
}

async function sendText(client: Anthropic, sessionId: string, text: string): Promise<any> {
  return client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

async function events(client: Anthropic, sessionId: string): Promise<any[]> {
  const observed: any[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    observed.push(event);
  }
  return observed;
}

async function waitForCommitted(sessionId: string, marker: string, minimum = 1): Promise<any[]> {
  return waitForValue(
    async () => {
      const response = await fetch(`${BASE}/v1/durable/threads/${sessionId}/messages`);
      const body = await response.json() as { messages?: any[] };
      assert.equal(response.status, 200, `committed-message snapshot failed: ${response.status}`);
      return body.messages ?? [];
    },
    (observed: any[]) =>
      (JSON.stringify(observed).match(new RegExp(marker, 'g')) ?? []).length >= minimum,
    `committed messages never contained ${marker}`,
    { timeoutMs: 20_000, pollMs: 25 },
  );
}

async function liveInboxSnapshot(sessionId: string): Promise<{ active?: boolean }> {
  const response = await fetch(`${BASE}/v1/awaken/sessions/${sessionId}/live-inbox`, {
    headers: { 'anthropic-beta': BETAS[0] },
  });
  const body = await response.json().catch(() => ({})) as { active?: boolean };
  assert.equal(response.status, 200, `live-inbox snapshot failed: ${response.status}`);
  return body;
}

async function waitForActiveInbox(sessionId: string): Promise<void> {
  await waitForValue(
    () => liveInboxSnapshot(sessionId),
    (body: { active?: boolean }) => body.active === true,
    `live inbox for ${sessionId} never became active`,
    { timeoutMs: 10_000, pollMs: 25 },
  );
}

async function waitForInactiveInbox(sessionId: string): Promise<void> {
  await waitForValue(
    () => liveInboxSnapshot(sessionId),
    (body: { active?: boolean }) => body.active === false,
    `live inbox for ${sessionId} never became inactive`,
    { timeoutMs: 10_000, pollMs: 25 },
  );
}

async function waitForDispatchStatus(
  sessionId: string,
  runId: string,
  status: string,
): Promise<void> {
  await waitForValue(
    async () => {
      const response = await fetch(`${BASE}/v1/durable/threads/${sessionId}/dispatches`);
      const body = await response.json().catch(() => ({})) as { dispatches?: any[] };
      assert.equal(response.status, 200, `dispatch snapshot failed: ${response.status}`);
      return body.dispatches ?? [];
    },
    (dispatches: any[]) => dispatches.some(
      (dispatch) => dispatch.run_id === runId && dispatch.status === status,
    ),
    `Run ${runId} never reached durable dispatch status ${status}`,
    { timeoutMs: 20_000, pollMs: 25 },
  );
}

async function waitForIdle(
  client: Anthropic,
  sessionId: string,
  stopReason: string,
): Promise<any[]> {
  return waitForValue(
    () => events(client, sessionId),
    (observed: any[]) => observed.some(
      (event) => event.type === 'session.status_idle' && event.stop_reason?.type === stopReason,
    ),
    `session ${sessionId} never reached idle/${stopReason}`,
    { timeoutMs: 20_000, pollMs: 25 },
  );
}

async function waitForPause(sessionId: string): Promise<{ status: number; body: any }> {
  return waitForValue(
    () => post(`/v1/durable/threads/${sessionId}/pause`, {}),
    (observed: { status: number; body: any }) => observed.status === 200,
    `pause was never accepted for ${sessionId}`,
    { timeoutMs: 10_000, pollMs: 25 },
  );
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-acp-control-'));
  const failureStorage = mkdtempSync(path.join(tmpdir(), 'awaken-acp-relaunch-failure-'));
  let server = spawnServer('acp-control', PORT, {
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
  }).server;
  try {
    await waitForPort(PORT, 180_000, server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

    const paused = await createAcpSession(client);
    // Cause/effect graph: C0 the durable Session is idle; C1 its exact User
    // receipt owns an ACP claim that is locally executing; C2 pause arrives while
    // that registration is current; C3 ordinary input resumes the committed
    // ManualPause boundary. Effects: E0
    // idle/awaiting/ended never advertises a live inbox; E1 C1 advertises the
    // Worker-owned inbox and pause returns the active Run id; E2 the Run settles
    // at its durable ManualPause boundary without fabricating an answerable
    // Managed tool event; E3 one resumed continuation commits the second ACP
    // marker; E4 a post-settlement pause is rejected; E5 C1's receipt is processed
    // only after the ManualPause boundary commits. Constraints: foreground
    // request lifetime and local-pool topology are not ownership; the official
    // Managed SDK remains the sole Session/event admission surface, while the
    // Awaken pause/resume extension owns its no-tool ManualPause vocabulary.
    // Decision rules:
    // P0=C0=>E0; P1=C1+C2=>E1+E2+E5; P2=P1+C3=>E3; P3=not C1=>E0+E4.
    assert.equal((await liveInboxSnapshot(paused.id)).active, false, 'P0/E0');
    const firstRun = sendText(client, paused.id, 'pause this ACP Run');
    await waitForActiveInbox(paused.id);
    const pause = await waitForPause(paused.id);
    assert.equal(pause.status, 200, JSON.stringify(pause.body));
    assert.equal(pause.body.paused, true);
    assert.ok(pause.body.run_id);
    const firstRunReceipt = (await firstRun).data[0];
    assert.equal(firstRunReceipt?.type, 'user.message', 'P1 exact paused User receipt');
    await waitForDispatchStatus(paused.id, pause.body.run_id, 'Awaiting');
    await waitForInactiveInbox(paused.id);
    assert.equal((await liveInboxSnapshot(paused.id)).active, false, 'P1/E0 after RAII release');
    const { events: pausedEvents } = await waitForSessionEventReceipt(
      client,
      paused.id,
      firstRunReceipt.id,
      BETAS,
      () => true,
      'P1 original User receipt to process at the durable ManualPause boundary',
    );
    assert.ok(
      !pausedEvents.some(
        (event: any) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'requires_action',
      ),
      `ManualPause must not fabricate a Managed tool action: ${JSON.stringify(pausedEvents)}`,
    );
    pass('HTTP pause reaches the active ACP attempt and commits a durable ManualPause boundary');

    const resume = await post(`/v1/durable/threads/${paused.id}/resume`, {
      text: 'resume after the durable pause',
    });
    assert.equal(resume.status, 200, JSON.stringify(resume.body));
    const resumed = JSON.stringify(await waitForCommitted(paused.id, 'ACP-SLOW-RUN', 2));
    assert.ok(resumed.includes('ACP-SLOW-RUN'), resumed);
    await waitForIdle(client, paused.id, 'end_turn');
    await waitForInactiveInbox(paused.id);
    const afterEnd = await post(`/v1/durable/threads/${paused.id}/pause`, {});
    assert.equal(afterEnd.status, 400, 'pause is live-only and fails closed after settlement');
    assert.equal((await liveInboxSnapshot(paused.id)).active, false, 'P3/E0');
    pass('ordinary Managed input resumes the paused ACP Run; a stale pause fails closed');

    await stopServer(server);
    server = spawnServer('acp-relaunch-failure', PORT, {
      SESSION_DEPLOYMENT_STORAGE_DIR: failureStorage,
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      AWAKEN_DISPATCH_DAEMON: '1',
    }).server;
    await waitForPort(PORT, 180_000, server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

    const continued = await createAcpSession(client);
    const run = sendText(client, continued.id, 'start a slow ACP Run');
    // Cause/effect graph: C1 an ACP Run is active, C2 the Awaken extension
    // receives queued text, and C3 replacement launch is deliberately failed.
    // Effects are E1 queue acceptance, E2 exactly one original ACP Run, and E3
    // a durable retries_exhausted terminal event containing the launch failure.
    // Decision rule A1: C1+C2+C3 -> E1+E2+E3. The namespaced extension is the
    // sole live-inbox contract; the Managed Agents namespace has no parallel
    // compatibility route. Constraint: the fixture uses durable Worker
    // execution, because direct ACP has no Worker-owned live inbox to advertise.
    await waitForActiveInbox(continued.id);
    const queued = await post(`/v1/awaken/sessions/${continued.id}/live-inbox`, {
      content: [{ type: 'text', text: 'continue on a replacement ACP process' }],
    });
    assert.equal(queued.status, 200, JSON.stringify(queued.body));
    const runReceipt = (await run).data[0];
    assert.equal(runReceipt?.type, 'user.message', 'A1 exact User Event receipt family');
    // Receipt rule A2: C4 the original command has an exact receipt and C3 the
    // replacement fails; E4 that receipt is processed before E3 is accepted;
    // K1 older Session history cannot satisfy the terminal oracle. Decision
    // A2=C3+C4=>E3+E4 through the one canonical receipt-scoped SDK observer.
    const failedObservation = await waitForSessionEventReceipt(
      client,
      continued.id,
      runReceipt.id,
      BETAS,
      ({ delta }: { delta: any[] }) => delta.some(
        (event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'retries_exhausted',
      ) && JSON.stringify(delta).includes('deliberate replacement launch failure'),
      `session ${continued.id} never processed its command into idle/retries_exhausted`,
      { timeoutMs: 20_000, pollMs: 25 },
    );
    const failedEvents: any[] = failedObservation.events;
    const finalIdle = failedEvents.filter((event) => event.type === 'session.status_idle').at(-1);
    assert.equal(finalIdle?.stop_reason?.type, 'retries_exhausted', JSON.stringify(failedEvents));
    assert.ok(JSON.stringify(failedEvents).includes('deliberate replacement launch failure'));
    assert.equal(JSON.stringify(failedEvents).match(/ACP-SLOW-RUN/g)?.length, 1);
    await waitForInactiveInbox(continued.id);
    assert.equal((await liveInboxSnapshot(continued.id)).active, false, 'A1 RAII release');
    pass('live continuation uses the canonical ACP relaunch seam and commits its launch failure');

    console.log('ACP CONTROL LIFECYCLE TS API E2E PASS.');
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(storage, { recursive: true, force: true });
    fs.rmSync(failureStorage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('ACP CONTROL LIFECYCLE TS API E2E FAIL:', error);
  process.exitCode = 1;
});
