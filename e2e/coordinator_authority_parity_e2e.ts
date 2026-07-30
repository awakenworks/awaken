// Coordinator authority parity across the two supported deployment shapes.
//
// Cause graph:
//   C1 = the Coordinator co-locates an execution pool
//   C2 = the Coordinator disables its pool and a registered Worker claims remotely
//   C3 = the Session root mutation reaches idle
//   C4 = the operation commit produces exactly one assistant fact
//   C5 = dispatch settlement removes the live delivery row
//   C6 = the authority process restarts over the same durable roots
//
// Decision table:
//   Rule  C1 C2 C3 C4 C5 C6 | effect
//   R1     1  0  1  1  1  1 | all-in-one facts survive restart
//   R2     0  1  1  1  1  1 | distributed facts survive restart
//   R3     *  *  0  *  *  * | no successful terminal projection
//   R4     *  *  1  0  *  * | no fabricated assistant result
//   R5     *  *  1  1  0  * | delivery remains live and the test fails
//   R6     *  *  1  1  1  0 | durability is not claimed
// R1/R2 are exercised here; Rust claim/CAS conformance owns the fail-closed R3-R6
// causes. The two successful rows must normalize to the same terminal facts.

import assert from 'node:assert/strict';
import type { ChildProcessWithoutNullStreams } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
// @ts-expect-error The shared E2E harness is intentionally JavaScript.
import { availablePort, spawnServer, stopServer, waitForPort } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const PREFERRED_PORT = Number(process.env.E2E_PORT ?? 39871);

type TerminalFacts = {
  assistantText: string;
  assistantCount: number;
  status: string;
};

async function waitForAssistant(base: string, threadId: string): Promise<any[]> {
  const deadline = Date.now() + 30_000;
  while (Date.now() <= deadline) {
    const response = await fetch(`${base}/v1/durable/threads/${threadId}/messages`);
    if (response.status === 200) {
      const messages = ((await response.json()) as any).messages ?? [];
      const assistant = messages.filter((message: any) => message.role === 'Assistant');
      if (assistant.length > 0) return assistant;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`committed assistant fact did not appear for ${threadId}`);
}

function terminalFacts(assistant: any[], status: string): TerminalFacts {
  return {
    assistantText: assistant
      .map((message) => message.text ?? '')
      .join(''),
    assistantCount: assistant.length,
    status,
  };
}

async function waitForSettled(base: string, sessionId: string): Promise<void> {
  const deadline = Date.now() + 30_000;
  while (Date.now() <= deadline) {
    const response = await fetch(`${base}/v1/durable/threads/${sessionId}/dispatches`);
    const responseText = await response.text();
    assert.equal(response.status, 200, `dispatch authority is readable: ${responseText}`);
    const dispatches = (JSON.parse(responseText) as any).dispatches ?? [];
    if (dispatches.length === 0) return;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`delivery authority did not settle for ${sessionId}`);
}

async function runTopology(remoteWorker: boolean, preferredPort: number): Promise<TerminalFacts> {
  const label = remoteWorker ? 'distributed' : 'all-in-one';
  const storage = mkdtempSync(path.join(tmpdir(), `awaken-coordinator-${label}-`));
  const threadId = `authority-parity-${label}-${process.pid}`;
  const port = await availablePort(preferredPort);
  const base = `http://127.0.0.1:${port}`;
  let coordinator: ChildProcessWithoutNullStreams | undefined;
  let worker: ChildProcessWithoutNullStreams | undefined;
  try {
    coordinator = spawnServer('echo', port, {
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      SESSION_DEPLOYMENT_STORAGE_DIR: storage,
      ...(remoteWorker ? { SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1' } : {}),
    }).server;
    await waitForPort(port, 180_000, coordinator);
    if (remoteWorker) {
      worker = spawnServer('echo', 0, {
        SESSION_DEPLOYMENT_INGRESS: 'durable',
        AWAKEN_UPSTREAM_URL: base,
        AWAKEN_SCENARIO_ROLE: 'worker',
        AWAKEN_HTTP_ADDR: '127.0.0.1:0',
      }).server;
      await new Promise((resolve) => setTimeout(resolve, 2_000));
    }

    const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const session = await client.beta.sessions.create({
      agent: 'assistant',
      environment_id: 'env_local',
      betas: BETAS,
    });
    const submitted = await fetch(`${base}/v1/durable/threads/${threadId}/submit_background`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ text: 'authority parity' }),
    });
    assert.equal(submitted.status, 200, `${label}: Coordinator accepts run intent: ${await submitted.text()}`);
    const assistant = await waitForAssistant(base, threadId);
    const retrieved = await client.beta.sessions.retrieve(session.id, { betas: BETAS }).withResponse();
    assert.equal(retrieved.data.status, 'idle', `${label}: Session root reaches idle`);
    const etag = retrieved.response.headers.get('etag');
    assert.ok(etag && Number.isSafeInteger(Number(etag.replaceAll('"', ''))), `${label}: numeric root revision`);
    const beforeRestart = terminalFacts(assistant, retrieved.data.status);
    assert.equal(beforeRestart.assistantCount, 1, `${label}: one committed assistant fact`);
    await waitForSettled(base, threadId);

    await Promise.all([
      worker ? stopServer(worker) : Promise.resolve(),
      stopServer(coordinator),
    ]);
    worker = undefined;
    coordinator = undefined;

    coordinator = spawnServer('echo', port, {
      SESSION_DEPLOYMENT_INGRESS: 'durable',
      SESSION_DEPLOYMENT_STORAGE_DIR: storage,
      SESSION_DEPLOYMENT_DISABLE_LOCAL_POOL: '1',
    }).server;
    await waitForPort(port, 180_000, coordinator);
    const restarted = new Anthropic({ apiKey: 'e2e-dummy', baseURL: base });
    const recovered = await restarted.beta.sessions.retrieve(session.id, { betas: BETAS });
    const afterRestart = terminalFacts(await waitForAssistant(base, threadId), recovered.status);
    assert.deepEqual(afterRestart, beforeRestart, `${label}: all three authorities recover one terminal projection`);
    await waitForSettled(base, threadId);
    return afterRestart;
  } finally {
    await Promise.all([
      worker ? stopServer(worker) : Promise.resolve(),
      coordinator ? stopServer(coordinator) : Promise.resolve(),
    ]);
    rmSync(storage, { recursive: true, force: true });
  }
}

async function main(): Promise<void> {
  const allInOne = await runTopology(false, PREFERRED_PORT);
  const distributed = await runTopology(true, PREFERRED_PORT + 1);
  assert.deepEqual(
    distributed,
    allInOne,
    'all-in-one and distributed deployment normalize to identical Coordinator terminal facts',
  );
  console.log('COORDINATOR AUTHORITY PARITY TS E2E PASS');
}

main().catch((error) => {
  console.error('COORDINATOR AUTHORITY PARITY TS E2E FAIL:', error);
  process.exitCode = 1;
});
