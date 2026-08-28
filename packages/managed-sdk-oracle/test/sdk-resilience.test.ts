import assert from 'node:assert/strict';
import test from 'node:test';

import Anthropic from '@anthropic-ai/sdk-current';

const betas = ['managed-agents-2026-04-01'] as const;
const baseURL = 'https://managed.invalid';

function json(body: unknown, status = 200, headers: Record<string, string> = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json', ...headers },
  });
}

test('official SDK preserves the Managed Anthropic error contract without retrying client faults', async () => {
  // Cause/effect decision table:
  // C1={400,401,403,404,409,429}, C2=maxRetries=0 -> E1 exact HTTP status,
  // E2 canonical error type/message, E3 one request. This separates server
  // compatibility from SDK retry policy and detects an HTML/proxy error body.
  for (const [status, kind] of [
    [400, 'invalid_request_error'],
    [401, 'authentication_error'],
    [403, 'permission_error'],
    [404, 'not_found_error'],
    [409, 'conflict_error'],
    [429, 'rate_limit_error'],
  ] as const) {
    let requests = 0;
    const client = new Anthropic({
      apiKey: 'test', baseURL, maxRetries: 0,
      fetch: async () => {
        requests += 1;
        return json({ type: 'error', error: { type: kind, message: `status ${status}` } }, status);
      },
    });
    await assert.rejects(
      () => client.beta.sessions.retrieve('missing', { betas: [...betas] }),
      (error: unknown) => {
        assert.ok(error instanceof Anthropic.APIError, `${status}: SDK APIError`);
        assert.equal(error.status, status, `${status}: E1`);
        assert.equal(error.error.type, 'error', `${status}: E2/envelope`);
        assert.equal(error.error.error.type, kind, `${status}: E2/type`);
        assert.equal(error.error.error.message, `status ${status}`, `${status}: E2/message`);
        return true;
      },
    );
    assert.equal(requests, 1, `${status}: E3`);
  }
});

test('official SDK owns bounded retries for transient failure and conflict', async () => {
  // Causes: R1 two retryable 500 responses then success; R2 a semantic 409.
  // Effects: the current official SDK performs its configured three attempts
  // for both classes. The Runtime performs no retry, so command identity must
  // be stable and the server's idempotency/CAS contract remains authoritative.
  let transientAttempts = 0;
  const retrying = new Anthropic({
    apiKey: 'test', baseURL, maxRetries: 2,
    fetch: async () => {
      transientAttempts += 1;
      if (transientAttempts < 3) {
        return json(
          { type: 'error', error: { type: 'api_error', message: 'transient' } },
          500,
          { 'retry-after-ms': '0' },
        );
      }
      return json({ data: [], has_more: false, next_page: null });
    },
  });
  const page = await retrying.beta.sessions.list({ betas: [...betas] });
  assert.deepEqual(page.data, [], 'R1/result');
  assert.equal(transientAttempts, 3, 'R1/attempt bound');

  let conflictAttempts = 0;
  const conflicting = new Anthropic({
    apiKey: 'test', baseURL, maxRetries: 2,
    fetch: async () => {
      conflictAttempts += 1;
      return json({
        type: 'error', error: { type: 'conflict_error', message: 'stale command' },
      }, 409);
    },
  });
  await assert.rejects(
    () => conflicting.beta.sessions.update('session', { title: 'changed', betas: [...betas] }),
    (error: unknown) => error instanceof Anthropic.APIError && error.status === 409,
  );
  assert.equal(conflictAttempts, 3, 'R2/current SDK retry contract');
});

test('official SDK paginator resumes by next_page without overlap', async () => {
  // Cause/effect graph: P1 first response has_more+next_page; P2 terminal page
  // has no cursor. Effect: the SDK issues page=cursor-1 exactly once and yields
  // the two resources in order without duplicates.
  const requests: URL[] = [];
  const client = new Anthropic({
    apiKey: 'test', baseURL, maxRetries: 0,
    fetch: async (input) => {
      const url = new URL(input instanceof Request ? input.url : input.toString());
      requests.push(url);
      return url.searchParams.get('page') === 'cursor-1'
        ? json({ data: [{ id: 'session-2' }], has_more: false, next_page: null })
        : json({ data: [{ id: 'session-1' }], has_more: true, next_page: 'cursor-1' });
    },
  });
  const ids: string[] = [];
  for await (const session of client.beta.sessions.list({ limit: 1, betas: [...betas] })) {
    ids.push(session.id);
  }
  assert.deepEqual(ids, ['session-1', 'session-2']);
  assert.equal(new Set(ids).size, ids.length, 'no overlap');
  assert.equal(requests.length, 2);
  assert.equal(requests[1]?.searchParams.get('page'), 'cursor-1');
});

test('official SDK forwards one idempotency key across its retry attempts', async () => {
  // C1 the caller supplies one command identity; C2 transport retries once.
  // E1 both attempts carry the same identity, allowing the server repository
  // to replay one durable fact rather than execute the command twice.
  const keys: Array<string | null> = [];
  const client = new Anthropic({
    apiKey: 'test', baseURL, maxRetries: 1,
    fetch: async (input, init) => {
      const request = new Request(input, init);
      keys.push(request.headers.get('idempotency-key'));
      if (keys.length === 1) {
        return json({ type: 'error', error: { type: 'api_error', message: 'retry' } }, 500, {
          'retry-after-ms': '0',
        });
      }
      return json({ id: 'session', type: 'session' });
    },
  });
  await client.beta.sessions.create(
    { agent: 'agent', environment_id: 'environment', betas: [...betas] },
    { headers: { 'idempotency-key': 'qualification-command' } },
  );
  assert.deepEqual(keys, ['qualification-command', 'qualification-command']);
});
