import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

const BASE_URL = 'http://127.0.0.1:38080';

/**
 * POST with an AbortController that fires after receiving the HTTP headers.
 * Avoids buffering the full SSE body for slow multi-round tool-calling agents.
 */
async function postAndCheckHeaders(
  url: string,
  body: object,
): Promise<{ status: number; contentType: string }> {
  const controller = new AbortController();
  const res = await fetch(`${BASE_URL}${url}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal: controller.signal,
  });
  const status = res.status;
  const contentType = res.headers.get('content-type') ?? '';
  controller.abort();
  return { status, contentType };
}

test.describe('thread lifecycle', () => {
  test('create and retrieve thread', async ({ request }) => {
    const createRes = await request.post('/v1/threads', {
      data: { title: 'Lifecycle Test' },
    });
    expect(createRes.ok()).toBeTruthy();
    const thread = await createRes.json();
    expect(thread.id).toBeTruthy();

    const getRes = await request.get(`/v1/threads/${thread.id}`);
    expect(getRes.ok()).toBeTruthy();
    const fetched = await getRes.json();
    expect(fetched.id).toBe(thread.id);
  });

  test('create thread and add messages', async ({ request }) => {
    const createRes = await request.post('/v1/threads', {
      data: { title: 'Messages Test' },
    });
    const thread = await createRes.json();

    const msgRes = await request.post(`/v1/threads/${thread.id}/messages`, {
      data: {
        messages: [{ role: 'user', content: 'Hello from test' }],
      },
    });
    // post_thread_messages returns 202 Accepted (triggers background run)
    expect(msgRes.status()).toBe(202);

    const listRes = await request.get(`/v1/threads/${thread.id}/messages`);
    expect(listRes.ok()).toBeTruthy();
    const body = await listRes.json();
    expect(body.messages).toBeDefined();
    expect(typeof body.total).toBe('number');
  });

  test('delete thread', async ({ request }) => {
    const createRes = await request.post('/v1/threads', {
      data: { title: 'Delete Test' },
    });
    const thread = await createRes.json();

    const deleteRes = await request.delete(`/v1/threads/${thread.id}`);
    // delete returns 204 No Content
    expect(deleteRes.status()).toBe(204);

    const getRes = await request.get(`/v1/threads/${thread.id}`);
    expect(getRes.status()).toBe(404);
  });

  test('patch thread title', async ({ request }) => {
    const createRes = await request.post('/v1/threads', {
      data: { title: 'Patch Test' },
    });
    const thread = await createRes.json();

    // PatchThreadPayload expects { title?, custom? } — not nested under metadata
    const patchRes = await request.patch(`/v1/threads/${thread.id}`, {
      data: { title: 'Updated Title' },
    });
    expect(patchRes.ok()).toBeTruthy();

    const getRes = await request.get(`/v1/threads/${thread.id}`);
    const updated = await getRes.json();
    expect(updated.metadata.title).toBe('Updated Title');
  });

  test('patch thread custom metadata', async ({ request }) => {
    const createRes = await request.post('/v1/threads', {
      data: { title: 'Custom Meta Test' },
    });
    const thread = await createRes.json();

    const patchRes = await request.patch(`/v1/threads/${thread.id}`, {
      data: { custom: { foo: 'bar', count: 42 } },
    });
    expect(patchRes.ok()).toBeTruthy();

    const getRes = await request.get(`/v1/threads/${thread.id}`);
    const updated = await getRes.json();
    expect(updated.metadata.custom.foo).toBe('bar');
    expect(updated.metadata.custom.count).toBe(42);
  });
});

test.describe('run lifecycle', () => {
  test('start run on thread and read SSE events', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Run Lifecycle' },
    });
    const thread = await threadRes.json();

    const runRes = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(runRes.ok()).toBeTruthy();
    const body = await runRes.text();
    expect(body).toContain('data:');
    const events = body.split('\n').filter(l => l.startsWith('data:'));
    expect(events.length).toBeGreaterThan(0);
  });

  test('list runs for thread', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'List Runs' },
    });
    const thread = await threadRes.json();

    // Start a run first so there is at least one
    await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });

    const listRes = await request.get(`/v1/threads/${thread.id}/runs`);
    expect(listRes.ok()).toBeTruthy();
    const body = await listRes.json();
    expect(body.items).toBeDefined();
  });

  test('run with travel agent on thread', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Travel Run' },
    });
    const thread = await threadRes.json();

    // Use fetch+abort to avoid buffering the full SSE stream from multi-round tool loop
    const { status, contentType } = await postAndCheckHeaders('/v1/runs', {
      agentId: 'travel',
      threadId: thread.id,
      messages: [{ role: 'user', content: 'Plan a trip to Rome' }],
    });
    expect(status).toBe(200);
    expect(contentType).toContain('text/event-stream');
  });
});

test.describe('protocol endpoints', () => {
  test('AG-UI run with thread persistence', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Test AG-UI' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body.length).toBeGreaterThan(0);
  });

  test('AI SDK chat with multiple messages', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: aiSdkTextMessages([
          { role: 'user', text: 'Hello' },
          { role: 'assistant', text: 'Hi there!' },
          { role: 'user', text: 'What can you do?' },
        ]),
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');
  });
});
