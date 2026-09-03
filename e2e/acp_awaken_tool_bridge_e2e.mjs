// Deterministic product E2E for ACP -> Awaken Session MCP -> durable HITL ->
// rooted write/read -> Artifact. It uses the real scenario-host, Runtime Host,
// Managed API, MCP exporter and filesystem tools; only the external ACP model
// loop is a deterministic fixture.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import {
  pass,
  spawnServer,
  stopServer,
  waitForPort,
  waitForSessionEventReceipt,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 39783);
const BASE = `http://127.0.0.1:${PORT}`;
const BETAS = ['managed-agents-2026-04-01'];
const STORAGE = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-tool-bridge-'));

async function listEvents(client, sessionId) {
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  return events;
}

async function begin(client) {
  const session = await client.beta.sessions.create({
    agent: 'acp-agent',
    environment_id: 'env_local',
    betas: BETAS,
  });
  const receipt = await client.beta.sessions.events.send(session.id, {
    events: [{ type: 'user.message', content: [{ type: 'text', text: 'write the governed output exactly once' }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string');
  const awaiting = await waitForSessionEventReceipt(
    client,
    session.id,
    receiptId,
    BETAS,
    ({ delta }) => delta.some((event) => event.type === 'agent.tool_use'
      && event.name === 'write'
      && event.evaluated_permission === 'ask')
      && delta.some((event) => event.type === 'session.status_idle'
        && event.stop_reason?.type === 'requires_action'),
    'ACP Session MCP write to reach durable approval',
  );
  const tool = awaiting.delta.find((event) => event.type === 'agent.tool_use' && event.name === 'write');
  assert.ok(tool);
  assert.deepEqual(tool.input, {
    path: '/mnt/session/outputs/acp-hitl.txt',
    content: 'AWAKEN-ACP-HITL-OK',
  }, 'Codex-style MCP envelope is normalized to exact Awaken write arguments');
  return { session, tool };
}

async function decide(client, sessionId, toolId, result) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{
      type: 'user.tool_confirmation',
      tool_use_id: toolId,
      result,
      ...(result === 'deny' ? { deny_message: 'operator rejected' } : {}),
    }],
    betas: BETAS,
  });
  return receipt.data[0]?.id;
}

async function main() {
  const environment = {
    SESSION_DEPLOYMENT_STORAGE_DIR: STORAGE,
    SESSION_DEPLOYMENT_INGRESS: 'durable',
  };
  let server = spawnServer('acp-tool-bridge', PORT, environment).server;
  try {
    await waitForPort(PORT, 180_000, server);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE, maxRetries: 0 });
    const allowed = await begin(client);

    const beforeRefresh = await listEvents(client, allowed.session.id);
    const afterRefresh = await listEvents(client, allowed.session.id);
    assert.deepEqual(afterRefresh, beforeRefresh, 'refresh reads the same committed pending tool');

    await stopServer(server);
    server = spawnServer('acp-tool-bridge', PORT, environment).server;
    await waitForPort(PORT, 180_000, server);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: BASE, maxRetries: 0 });
    const recovered = await listEvents(client, allowed.session.id);
    assert.ok(recovered.some((event) => event.id === allowed.tool.id), 'restart retains the exact pending tool');

    const allowReceipt = await decide(client, allowed.session.id, allowed.tool.id, 'allow');
    assert.equal(typeof allowReceipt, 'string');
    const completed = await waitForSessionEventReceipt(
      client,
      allowed.session.id,
      allowReceipt,
      BETAS,
      ({ delta }) => JSON.stringify(delta).includes('ACP-BRIDGE-ALLOWED-READBACK-OK')
        && delta.some((event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'end_turn'),
      'approved ACP bridge to execute and read back the rooted write',
      { timeoutMs: 120_000 },
    );
    assert.equal(
      completed.events.filter((event) => event.type === 'agent.tool_use' && event.id === allowed.tool.id).length,
      1,
      'the pending write occurrence remains unique',
    );
    const artifacts = await client.beta.files.list({ scope_id: allowed.session.id, betas: BETAS });
    assert.ok(
      artifacts.data.some((entry) => entry.filename === 'acp-hitl.txt'),
      `approved output is harvested as an Artifact: ${JSON.stringify(artifacts.data)}`,
    );
    await assert.rejects(
      decide(client, allowed.session.id, allowed.tool.id, 'allow'),
      (error) => error?.status === 400 || error?.status === 409,
      'a duplicate approval cannot execute the side effect again',
    );
    pass('ACP approve survives refresh/restart, writes once, reads back, and publishes an Artifact');

    const denied = await begin(client);
    const denyReceipt = await decide(client, denied.session.id, denied.tool.id, 'deny');
    const deniedEnd = await waitForSessionEventReceipt(
      client,
      denied.session.id,
      denyReceipt,
      BETAS,
      ({ delta }) => JSON.stringify(delta).includes('ACP-BRIDGE-DENIED-NO-EFFECT')
        && delta.some((event) => event.type === 'session.status_idle'
          && event.stop_reason?.type === 'end_turn'),
      'denied ACP bridge to terminate without dispatch',
    );
    assert.ok(
      deniedEnd.events.some((event) => event.type === 'agent.message'
        && JSON.stringify(event.content).includes('ACP-BRIDGE-DENIED-NO-EFFECT')),
      'the denied ACP receives an explicit model-visible result',
    );
    const deniedArtifacts = await client.beta.files.list({ scope_id: denied.session.id, betas: BETAS });
    assert.ok(!deniedArtifacts.data.some((entry) => entry.filename === 'acp-hitl.txt'));
    pass('ACP deny returns a model-visible result and creates no file or Artifact');

    console.log('E2E PASS: ACP Awaken tool bridge durable approval, denial, restart and Artifact coverage.');
  } finally {
    await stopServer(server).catch(() => {});
    fs.rmSync(STORAGE, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
