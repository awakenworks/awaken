// Durable Skill bundle + Session pin end to end.
//
// This drives the production management composition: the edge resolves the
// Workspace and enforces resource actions, while the Skill repository sees only
// that trusted Workspace coordinate. It proves binary-safe bundles, one-time
// Session resolution, logical version retirement, and restart rehydration.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import Anthropic from '@anthropic-ai/sdk';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38237);
const BETAS = ['managed-agents-2026-04-01'];
const SEAL_KEY = '00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff';
const AGENT = 'skill-pin-agent';
const V1 = 'PINNED_SKILL_V1_7119';
const V2 = 'CURRENT_SKILL_V2_8120';
const BINARY = Uint8Array.from([0, 159, 146, 150, 255, 13, 0, 10]);

function skillMarkdown(marker) {
  return `---\nname: greet\ndescription: deterministic pinned skill\n---\n${marker}`;
}

async function request(baseUrl, method, route, body) {
  const response = await fetch(`${baseUrl}${route}`, {
    method,
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const bytes = new Uint8Array(await response.arrayBuffer());
  const text = new TextDecoder().decode(bytes);
  let value = text;
  try { value = JSON.parse(text); } catch {}
  return { status: response.status, body: value, bytes };
}

async function uploadBundle(baseUrl, route, marker, binary = undefined) {
  const form = new FormData();
  form.append('file', new Blob([skillMarkdown(marker)], { type: 'text/markdown' }), 'SKILL.md');
  if (binary !== undefined) {
    form.append('file', new Blob([binary], { type: 'application/octet-stream' }), 'assets/data.bin');
  }
  const response = await fetch(`${baseUrl}${route}`, { method: 'POST', body: form });
  const body = await response.json().catch(() => ({}));
  assert.equal(response.status, 200, `${route}: ${JSON.stringify(body)}`);
  return body;
}

async function publishAgent(baseUrl, skillId) {
  const config = {
    id: AGENT,
    system: 'Use the selected skill.',
    max_steps: 8,
    model: { id: 'management-skills' },
    tools: [],
    plugins: [],
    plugin_config: {},
    skills: [{ id: skillId }],
  };
  const put = await request(baseUrl, 'PUT', `/v1/config/agents/${AGENT}`, config);
  assert.equal(put.status, 200, `agent config stored: ${JSON.stringify(put.body)}`);
  const published = await request(baseUrl, 'POST', `/v1/config/agents/${AGENT}/publish`);
  assert.equal(published.status, 200, `agent config published: ${JSON.stringify(published.body)}`);
  assert.equal(published.body.installed, true, 'agent publication installed');
}

async function createSession(client) {
  return client.beta.sessions.create({
    agent: AGENT,
    environment_id: 'env_local',
    betas: BETAS,
  });
}

async function runAndReadLastReply(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const events = [];
  for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
    events.push(event);
  }
  const replies = events.filter((event) => event.type === 'agent.message');
  assert.ok(replies.length > 0, 'session emitted an agent message');
  return JSON.stringify(replies.at(-1).content);
}

async function main() {
  const managementDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-skill-pin-mgmt-'));
  const storageDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-skill-pin-store-'));
  const env = {
    AWAKEN_MGMT_DIR: managementDir,
    AWAKEN_MGMT_SEAL_KEY: SEAL_KEY,
    AWAKEN_STORAGE_DIR: storageDir,
    AWAKEN_STORE: 'fs',
  };
  let server = null;
  try {
    // Lifetime A: publish v1, freeze it into a Session, then publish v2.
    const first = spawnServer('management-skills', PORT, env);
    server = first.server;
    await waitForPort(PORT);
    let client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: first.baseUrl });

    const created = await uploadBundle(first.baseUrl, '/v1/skills', V1, BINARY);
    const skillId = created.id;
    assert.match(skillId, /^skill_[0-9a-f]{16}$/u, 'multipart upload returned a stable catalog id');

    const binary = await request(
      first.baseUrl,
      'GET',
      `/v1/skills/${skillId}/versions/1/files/assets/data.bin`,
    );
    assert.equal(binary.status, 200, 'support file is retrievable');
    assert.deepEqual(binary.bytes, BINARY, 'support file is binary-safe');
    pass('full binary Skill bundle persisted without UTF-8 coercion');

    await publishAgent(first.baseUrl, skillId);
    const pinned = await createSession(client);
    const firstReply = await runAndReadLastReply(client, pinned.id, 'use the frozen skill');
    assert.ok(firstReply.includes(V1), `the Session resolved Skill v1: ${firstReply}`);

    const version2 = await uploadBundle(
      first.baseUrl,
      `/v1/skills/${skillId}/versions`,
      V2,
    );
    assert.equal(version2.version, '2', 'v2 appended monotonically');
    const retired = await request(
      first.baseUrl,
      'DELETE',
      `/v1/skills/${skillId}/versions/1`,
    );
    assert.equal(retired.status, 200, `v1 retired: ${JSON.stringify(retired.body)}`);
    const hidden = await request(first.baseUrl, 'GET', `/v1/skills/${skillId}/versions/1`);
    assert.equal(hidden.status, 404, 'retired v1 is hidden from the management API');
    pass('v1 retired from management views after v2 publication');

    // Lifetime B: old Session reloads its durable v1 pin; a new Session selects v2.
    await stopServer(first.server);
    server = null;
    const second = spawnServer('management-skills', PORT, env);
    server = second.server;
    await waitForPort(PORT);
    client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: second.baseUrl });

    const restoredReply = await runAndReadLastReply(client, pinned.id, 'use it again after restart');
    assert.ok(restoredReply.includes(V1), 'rehydrated old Session still loads retained v1 bytes');
    assert.ok(!restoredReply.includes(V2), 'old Session did not drift to current v2');

    const current = await createSession(client);
    const currentReply = await runAndReadLastReply(client, current.id, 'use the current skill');
    assert.ok(currentReply.includes(V2), 'new Session resolves current v2');
    assert.ok(!currentReply.includes(V1), 'new Session does not select retired v1');
    pass('restart preserves old Session pin while a new Session selects v2');

    console.log('E2E PASS: binary Skill bundle + exact Session version pin + retirement/restart.');
  } finally {
    if (server) await stopServer(server);
    fs.rmSync(managementDir, { recursive: true, force: true });
    fs.rmSync(storageDir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
