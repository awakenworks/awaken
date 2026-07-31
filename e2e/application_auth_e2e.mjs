// Complete phase-one application-authentication flow over the production
// management composition: service credential -> short-lived application token
// -> explicit Managed Session binding -> official Vercel AI SDK client.

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

async function createSession(base, serviceToken, title) {
  const response = await request(
    base,
    'POST',
    '/v1/sessions',
    { agent: 'assistant', title },
    serviceToken,
  );
  assert.equal(response.status, 200, `create Managed Session: ${response.text}`);
  assert.ok(response.body.id.startsWith('sesn_'));
  return response.body;
}

async function mint(base, serviceToken, scope, managedSessionId, externalThreadId = 'shared') {
  const response = await request(
    base,
    'POST',
    '/v1/application-access-tokens',
    {
      authority_id: 'e2e-customer-backend',
      application_scope: scope,
      actor_key: 'opaque-e2e-user',
      protocols: ['ai-sdk'],
      operations: ['thread.run', 'thread.messages.read'],
      thread_bindings: [{
        external_thread_id: externalThreadId,
        managed_session_id: managedSessionId,
      }],
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

    const sessionA = await createSession(base, serviceToken, 'project A');
    const sessionB = await createSession(base, serviceToken, 'project B');
    const beforeRuns = await request(base, 'GET', '/v1/sessions', undefined, serviceToken);
    assert.equal(beforeRuns.status, 200, beforeRuns.text);
    const sessionIdsBeforeRuns = beforeRuns.body.data.map((session) => session.id).sort();

    const projectA = await mint(base, serviceToken, 'project-a', sessionA.id);
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

    const projectB = await mint(base, serviceToken, 'project-b', sessionB.id);
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
    pass('the same external thread id resolves to each token\'s explicit Managed Session binding');

    const afterRuns = await request(base, 'GET', '/v1/sessions', undefined, serviceToken);
    assert.equal(afterRuns.status, 200, afterRuns.text);
    assert.deepEqual(
      afterRuns.body.data.map((session) => session.id).sort(),
      sessionIdsBeforeRuns,
      'application protocol runs must not create a second Session',
    );
    assert.ok(afterRuns.body.data.every((session) => !session.id.startsWith('app_')));
    pass('AI SDK runs reuse the two pre-existing Managed Sessions without app_* duplicates');

    response = await request(
      base,
      'POST',
      '/v1/ag-ui',
      { threadId: 'shared', messages: [] },
      projectA.access_token,
    );
    assert.equal(response.status, 403);
    pass('an AI SDK token cannot be replayed against AG-UI');

    response = await request(
      base,
      'POST',
      '/v1/ai-sdk/agents/not-allowed/runs',
      { threadId: 'shared', messages: [] },
      projectA.access_token,
    );
    assert.equal(response.status, 403);
    pass('the frozen Managed Session Agent cannot be overridden by a request');

    response = await request(
      base,
      'POST',
      '/v1/ai-sdk/threads/unbound/runs',
      { messages: [] },
      projectA.access_token,
    );
    assert.equal(response.status, 403);
    pass('an unbound external thread cannot fall back to a derived Session');

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

    console.log('E2E PASS: service-key -> Managed Session -> bound application-token -> AI SDK.');
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
