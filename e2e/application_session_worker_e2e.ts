// ADR-0066 application contribution / realization over two real processes.
//
// Cause graph:
//   C1 = a registered Worker owns the claimed Run
//   C2 = the Worker application returns one frozen plan
//   C3 = Control accepts that plan into the Session aggregate
//   C4 = the exact realization generation is staged/activated/acknowledged
//   C5 = the coordinator-only cell enqueues without a local claim loop
//   C6 = authenticated remote settle wakes the foreground request from committed truth
//   C7 = continuing Worker authority remains provable near lease expiry
//   C8 = continuing Worker authority is later lost
//   C9 = application network policy uses the neutral provisioning vocabulary
//
// Decision table:
//   C1 C2 C3 C4 C5 C6 C7 C8 | result
//    1  1  1  1  1  1  1  0 | exact generation is renewed and republished
//    0  *  *  *  1  *  *  * | worker transport rejects the request (worker_transport)
//    1  1  0  *  1  *  *  * | no runtime projection / no successful turn
//    1  1  1  0  1  *  *  * | realization fails closed / no successful turn
//    1  1  1  1  0  *  *  * | local-pool topology uses the same dispatch (durable suites)
//    1  1  1  1  1  0  *  * | committed state remains authoritative; no fabricated success
//    1  1  1  1  1  1  1  1 | heartbeat fails closed; local Session projection is revoked
//    1  1  1  1  1  1  1  0 + C9 | policy freezes without a parallel vocabulary/path

import assert from 'node:assert/strict';
import { execFileSync, spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import Anthropic from '@anthropic-ai/sdk';
// @ts-expect-error The shared E2E harness is intentionally JavaScript.
import { spawnServer, stopServer, waitForPort } from './harness.mjs';
// @ts-expect-error The shared MCP fixture is intentionally JavaScript.
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const PORT = Number(process.env.E2E_PORT ?? 39851);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];

function etagRevision(value: string | null): number {
  assert.ok(value, 'Session response carries an ETag');
  const revision = Number(value.replaceAll('"', ''));
  assert.ok(Number.isSafeInteger(revision), `numeric Session ETag: ${value}`);
  return revision;
}

function buildWorker(): string {
  const output = execFileSync(
    'cargo',
    ['build', '--quiet', '--message-format=json', '-p', 'awaken-worker', '--example', 'application_session_worker'],
    { cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 },
  );
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'application_session_worker') {
        return message.executable;
      }
    } catch {
      // Cargo diagnostics are not artifact records.
    }
  }
  throw new Error('could not resolve application_session_worker example');
}

async function events(client: Anthropic, sessionId: string): Promise<any[]> {
  const observed: any[] = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    observed.push(event);
  }
  return observed;
}

async function waitForProjection(client: Anthropic, sessionId: string): Promise<any[]> {
  const deadline = Date.now() + 30_000;
  let observed: any[] = [];
  while (Date.now() <= deadline) {
    observed = await events(client, sessionId);
    const managedText = observed
      .filter((event) => event.type === 'agent.message')
      .flatMap((event) => event.content ?? [])
      .map((part) => part.text ?? '')
      .join('');
    const durableResponse = await fetch(`${BASE}/v1/durable/threads/${sessionId}/messages`);
    const durable = durableResponse.status === 200
      ? ((await durableResponse.json()) as any).messages ?? []
      : [];
    const durableText = durable.map((message: any) => message.text ?? '').join('');
    if (`${managedText}${durableText}`.includes('application-session-projection:visible')) {
      return [...observed, ...durable];
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`application projection never became visible: ${JSON.stringify(observed)}`);
}

async function main(): Promise<void> {
  const storage = mkdtempSync(path.join(tmpdir(), 'awaken-application-session-'));
  const applicationMcp = await startCalcFixture(undefined, { allowAnonymous: true });
  const sessionMcp = await startCalcFixture(undefined, { allowAnonymous: true });
  const cell = spawnServer('echo', PORT, {
    SESSION_DEPLOYMENT_INGRESS: 'durable',
    SESSION_DEPLOYMENT_STORAGE_DIR: storage,
    SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
  }).server;
  let cellStopped = false;
  let worker: ChildProcessWithoutNullStreams | undefined;
  let workerOutput = '';
  try {
    await waitForPort(PORT);
    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      application_contribution_required: true,
      mcp_servers: [{ name: 'application-calc', type: 'url', url: sessionMcp.url }],
      betas: BETAS,
    } as any);
    assert.equal(session.status, 'preparing', 'required application leaves one durable preparation intent');
    const binary = buildWorker();
    const runningWorker = spawn(binary, [], {
      cwd: ROOT,
      env: {
        ...process.env,
        AWAKEN_UPSTREAM_URL: BASE,
        AWAKEN_WORKER_ID: `application-session-worker-${process.pid}`,
        AWAKEN_TEST_MCP_URL: applicationMcp.url,
      },
      stdio: ['pipe', 'pipe', 'pipe'],
    });
    worker = runningWorker;
    runningWorker.stdout.on('data', (chunk) => { workerOutput += chunk.toString(); });
    runningWorker.stderr.on('data', (chunk) => { workerOutput += chunk.toString(); });
    await new Promise((resolve) => setTimeout(resolve, 500));
    try {
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'exercise application contribution' }] }],
        betas: BETAS,
      });
    } catch (error) {
      throw new Error(`${error}\nWorker output:\n${workerOutput}`);
    }
    let observed: any[];
    try {
      observed = await waitForProjection(client, session.id);
    } catch (error) {
      const [dispatches, currentSession] = await Promise.all([
        fetch(`${BASE}/v1/durable/threads/${session.id}/dispatches`).then((response) => response.text()),
        client.beta.sessions.retrieve(session.id, { betas: BETAS }).catch((cause) => ({ cause: String(cause) })),
      ]);
      throw new Error(
        `${error}\ndispatches: ${dispatches}\nsession: ${JSON.stringify(currentSession)}`
        + `\nWorker output:\n${workerOutput}`,
      );
    }
    assert.ok(
      observed.some((event) =>
        event.type === 'agent.message'
        || String(event.text ?? '').includes('application-session-projection:visible')),
      'the claim-fenced application projection produced one committed model turn',
    );
    const realizedResponse = await client.beta.sessions
      .retrieve(session.id, { betas: BETAS })
      .withResponse();
    const realized = realizedResponse.data;
    const realizedEtag = realizedResponse.response.headers.get('etag');
    assert.ok(realizedEtag, 'realized Session exposes its root revision');
    const realizedRevision = etagRevision(realizedEtag);
    assert.equal(realized.status, 'idle', 'realization acknowledgement projects durable idle state');
    assert.equal(
      realized.environment_id,
      'env_local',
      'C9 the application network policy preserves the one frozen Environment identity',
    );
    assert.deepEqual(
      realized.agent.mcp_servers,
      [
        { name: 'application-calc', type: 'url', url: sessionMcp.url },
        { name: 'application-only', type: 'url', url: applicationMcp.url },
      ],
      'Session origin overrides the same-name Application input while the other Application attachment remains',
    );
    assert.ok(
      applicationMcp.calls.some((call: any) => call.method === 'initialize')
      && sessionMcp.calls.some((call: any) => call.method === 'initialize'),
      'the remote Worker stages and publishes both selected MCP generations',
    );
    assert.equal(runningWorker.exitCode, null, `Worker stayed authoritative: ${workerOutput}`);

    // Let foreground completion writes settle before measuring the independent
    // Session-lease supervisor. The initial registry lease is not due at the
    // first 10-second heartbeat; the second heartbeat crosses the 15-second
    // renewal threshold.
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    const settled = await client.beta.sessions
      .retrieve(session.id, { betas: BETAS })
      .withResponse();
    const settledRevision = etagRevision(settled.response.headers.get('etag'));
    assert.ok(settledRevision >= realizedRevision, 'foreground Session writes are monotonic');
    await new Promise((resolve) => setTimeout(resolve, 22_000));
    const renewed = await client.beta.sessions
      .retrieve(session.id, { betas: BETAS })
      .withResponse();
    const renewedRevision = etagRevision(renewed.response.headers.get('etag'));
    assert.equal(renewed.data.status, 'idle', 'renewal does not reopen Session activation');
    assert.ok(
      renewedRevision >= settledRevision + 2,
      `C7 the due Session lease advances through the same realization CAS protocol: ${workerOutput}`,
    );

    await stopServer(cell);
    cellStopped = true;
    const authorityDeadline = Date.now() + 15_000;
    while (
      Date.now() < authorityDeadline
      && !workerOutput.includes('worker heartbeat cannot prove continuing authority')
    ) {
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    assert.match(
      workerOutput,
      /worker heartbeat cannot prove continuing authority/,
      'C8 loss of Control authority closes claims and revokes the local Session projection',
    );
    console.log('APPLICATION SESSION WORKER TS E2E PASS: factory -> claim-fenced contribution -> exact realization -> prompt-visible model turn.');
  } finally {
    if (worker) await stopServer(worker);
    if (!cellStopped) await stopServer(cell);
    await applicationMcp.close();
    await sessionMcp.close();
    rmSync(storage, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('APPLICATION SESSION WORKER TS E2E FAIL:', error);
  process.exitCode = 1;
});
