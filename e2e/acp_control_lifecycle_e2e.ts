// Served-process coverage for ACP cooperative pause/resume and continuation
// relaunch failure. Every control enters through HTTP; the only fixture is the
// external newline ACP subprocess composed by awaken-scenario-host.

import assert from 'node:assert/strict';
import fs, { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { pass, spawnServer, stopServer, waitForPort } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39772);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

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

async function sendText(client: Anthropic, sessionId: string, text: string): Promise<void> {
  await client.beta.sessions.events.send(sessionId, {
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
  const deadline = Date.now() + 20_000;
  let observed: any[] = [];
  while (Date.now() <= deadline) {
    const response = await fetch(`${BASE}/v1/durable/threads/${sessionId}/messages`);
    const body = await response.json();
    observed = body.messages ?? [];
    if ((JSON.stringify(observed).match(new RegExp(marker, 'g')) ?? []).length >= minimum) return observed;
    await sleep(25);
  }
  throw new Error(`committed messages never contained ${marker}: ${JSON.stringify(observed)}`);
}

async function waitForActiveInbox(sessionId: string): Promise<void> {
  const deadline = Date.now() + 10_000;
  while (Date.now() <= deadline) {
    const response = await fetch(`${BASE}/v1/awaken/sessions/${sessionId}/live-inbox`, {
      headers: { 'anthropic-beta': BETAS[0] },
    });
    const body = await response.json().catch(() => ({}));
    assert.equal(response.status, 200, `live-inbox snapshot failed: ${response.status}`);
    if (body.active) return;
    await sleep(25);
  }
  throw new Error(`live inbox for ${sessionId} never became active`);
}

async function waitForPause(sessionId: string): Promise<{ status: number; body: any }> {
  const deadline = Date.now() + 10_000;
  let observed = { status: 0, body: {} as any };
  while (Date.now() <= deadline) {
    observed = await post(`/v1/durable/threads/${sessionId}/pause`, {});
    if (observed.status === 200) return observed;
    await sleep(25);
  }
  throw new Error(`pause was never accepted for ${sessionId}: ${JSON.stringify(observed)}`);
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-acp-control-'));
  let server = spawnServer('acp-control', PORT, {
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    AWAKEN_DISPATCH_DAEMON: '1',
  }).server;
  try {
    await waitForPort(PORT, 180_000, server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

    const paused = await createAcpSession(client);
    const firstTurn = sendText(client, paused.id, 'pause this ACP turn');
    await waitForActiveInbox(paused.id);
    const pause = await waitForPause(paused.id);
    assert.equal(pause.status, 200, JSON.stringify(pause.body));
    assert.equal(pause.body.paused, true);
    assert.ok(pause.body.run_id);
    await firstTurn;
    const pausedEvents = await events(client, paused.id);
    const pausedIdle = pausedEvents.filter((event) => event.type === 'session.status_idle').at(-1);
    assert.equal(pausedIdle?.stop_reason?.type, 'requires_action', JSON.stringify(pausedEvents));
    pass('HTTP pause reaches the active ACP attempt and commits a durable ManualPause boundary');

    const resume = await post(`/v1/durable/threads/${paused.id}/resume`, {
      text: 'resume after the durable pause',
    });
    assert.equal(resume.status, 200, JSON.stringify(resume.body));
    const resumed = JSON.stringify(await waitForCommitted(paused.id, 'ACP-SLOW-TURN', 2));
    assert.ok(resumed.includes('ACP-SLOW-TURN'), resumed);
    const afterEnd = await post(`/v1/durable/threads/${paused.id}/pause`, {});
    assert.equal(afterEnd.status, 400, 'pause is live-only and fails closed after settlement');
    pass('ordinary Managed input resumes the paused ACP Run; a stale pause fails closed');

    await stopServer(server);
    server = spawnServer('acp-relaunch-failure', PORT, {}).server;
    await waitForPort(PORT, 180_000, server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });

    const continued = await createAcpSession(client);
    const turn = sendText(client, continued.id, 'start a slow ACP turn');
    // Cause/effect graph: C1 an ACP turn is active, C2 the Awaken extension
    // receives queued text, and C3 replacement launch is deliberately failed.
    // Effects are E1 queue acceptance, E2 exactly one original ACP turn, and E3
    // a durable retries_exhausted terminal event containing the launch failure.
    // Decision rule A1: C1+C2+C3 -> E1+E2+E3. The namespaced extension is the
    // sole live-inbox contract; the Managed Agents namespace has no parallel
    // compatibility route.
    await waitForActiveInbox(continued.id);
    const queued = await post(`/v1/awaken/sessions/${continued.id}/live-inbox`, {
      content: [{ type: 'text', text: 'continue on a replacement ACP process' }],
    });
    assert.equal(queued.status, 200, JSON.stringify(queued.body));
    await turn;
    const failedEvents = await events(client, continued.id);
    const finalIdle = failedEvents.filter((event) => event.type === 'session.status_idle').at(-1);
    assert.equal(finalIdle?.stop_reason?.type, 'retries_exhausted', JSON.stringify(failedEvents));
    assert.ok(JSON.stringify(failedEvents).includes('deliberate replacement launch failure'));
    assert.equal(JSON.stringify(failedEvents).match(/ACP-SLOW-TURN/g)?.length, 1);
    pass('live continuation uses the canonical ACP relaunch seam and commits its launch failure');

    console.log('ACP CONTROL LIFECYCLE TS API E2E PASS.');
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('ACP CONTROL LIFECYCLE TS API E2E FAIL:', error);
  process.exitCode = 1;
});
