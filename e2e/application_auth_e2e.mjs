// Complete phase-one application-authentication flow over the production
// management composition: service credential -> short-lived application token
// -> official Vercel AI SDK client -> isolated Awaken thread.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Chat } from '@ai-sdk/react';
import { DefaultChatTransport } from 'ai';
import {
  deploymentEnv,
  pass,
  realServerEnv,
  spawnServer,
  startUpstream,
  stopServer,
  waitForPort,
} from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38642);
const SEAL_KEY = 'ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100';

async function request(base, method, route, body, token) {
  const headers = {};
  if (body !== undefined) headers['content-type'] = 'application/json';
  if (token) headers.authorization = `Bearer ${token}`;
  const response = await fetch(`${base}${route}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return {
    status: response.status,
    body: text ? JSON.parse(text) : null,
    text,
  };
}

async function mint(base, serviceToken, scope, agents = ['assistant']) {
  const response = await request(
    base,
    'POST',
    '/v1/application-access-tokens',
    {
      authority_id: 'e2e-customer-backend',
      application_scope: scope,
      thread_namespace: 'customer-chat',
      actor_key: 'opaque-e2e-user',
      operations: ['thread.run', 'thread.read'],
      agent_ids: agents,
      default_agent_id: agents[0],
      expires_in_seconds: 300,
    },
    serviceToken,
  );
  assert.equal(response.status, 201, `mint application token: ${response.text}`);
  assert.ok(response.body.access_token.startsWith('sk-awaken-'));
  return response.body;
}

function chat(base, threadId, token) {
  return new Chat({
    id: threadId,
    transport: new DefaultChatTransport({
      api: `${base}/v1/ai-sdk/threads/${threadId}/runs`,
      headers: { authorization: `Bearer ${token}` },
    }),
  });
}

function assistantText(client) {
  return (client.lastMessage?.parts ?? [])
    .filter((part) => part.type === 'text')
    .map((part) => part.text)
    .join('');
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-app-auth-e2e-'));
  const upstream = await startUpstream('default');
  const env = {
    ...deploymentEnv(dir, { identityMode: 'self-managed', controlSealKey: SEAL_KEY }),
    ...realServerEnv('default', upstream, { mode: 'management' }),
  };
  const { server, baseUrl: base } = spawnServer('management', PORT, env);
  try {
    await waitForPort(PORT, 180_000, server);
    const serviceToken = fs.readFileSync(path.join(dir, 'admin-token'), 'utf8').trim();

    let response = await request(base, 'GET', '/v1/sessions');
    assert.equal(response.status, 401);
    response = await request(base, 'GET', '/v1/sessions', undefined, serviceToken);
    assert.equal(response.status, 200, response.text);
    pass('Managed Agents requires and accepts the workspace service credential');

    const projectA = await mint(base, serviceToken, 'project-a');
    response = await request(
      base,
      'POST',
      '/v1/ai-sdk/threads/shared/runs',
      { messages: [] },
      serviceToken,
    );
    assert.equal(response.status, 401, 'a service key is not an application token');
    response = await request(base, 'GET', '/v1/sessions', undefined, projectA.access_token);
    assert.equal(response.status, 401, 'an application token is not a service key');
    pass('service and application credentials cannot be substituted');

    const clientA = chat(base, 'shared', projectA.access_token);
    await clientA.sendMessage({ text: 'message from project A' });
    assert.match(assistantText(clientA), /message from project A/);
    pass('official AI SDK Chat streams through the application-authenticated route');

    const projectB = await mint(base, serviceToken, 'project-b');
    const clientB = chat(base, 'shared', projectB.access_token);
    await clientB.sendMessage({ text: 'message from project B' });
    assert.match(assistantText(clientB), /message from project B/);

    const historyA = await request(
      base,
      'GET',
      '/v1/ai-sdk/threads/shared/messages',
      undefined,
      projectA.access_token,
    );
    const historyB = await request(
      base,
      'GET',
      '/v1/ai-sdk/threads/shared/messages',
      undefined,
      projectB.access_token,
    );
    assert.equal(historyA.status, 200, historyA.text);
    assert.equal(historyB.status, 200, historyB.text);
    assert.match(historyA.text, /message from project A/);
    assert.doesNotMatch(historyA.text, /message from project B/);
    assert.match(historyB.text, /message from project B/);
    assert.doesNotMatch(historyB.text, /message from project A/);
    pass('the same external thread id is isolated by opaque application scope');

    response = await request(
      base,
      'POST',
      '/v1/ai-sdk/agents/not-allowed/runs',
      { threadId: 'agent-denied', messages: [] },
      projectA.access_token,
    );
    assert.equal(response.status, 403);
    pass('Agent allow-list rejects an out-of-scope Agent');

    response = await request(
      base,
      'DELETE',
      `/v1/application-access-tokens/${projectA.id}`,
      undefined,
      serviceToken,
    );
    assert.equal(response.status, 204, response.text);
    response = await request(
      base,
      'GET',
      '/v1/ai-sdk/threads/shared/messages',
      undefined,
      projectA.access_token,
    );
    assert.equal(response.status, 401);
    pass('revocation immediately invalidates the application token');

    console.log('E2E PASS: complete service-key -> application-token -> AI SDK flow.');
  } finally {
    await stopServer(server);
    upstream.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
