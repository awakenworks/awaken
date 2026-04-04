import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

const BASE_URL = 'http://127.0.0.1:38080';

/**
 * POST with an AbortController that fires after receiving the HTTP headers.
 * This avoids buffering the full SSE body for slow multi-round tool-calling agents.
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
  // Abort immediately — we only needed the headers
  controller.abort();
  return { status, contentType };
}

test.describe('multi-agent variants', () => {
  test('default agent accepts run', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    expect(res.headers()['content-type']).toContain('text/event-stream');
  });

  test('travel agent accepts run', async () => {
    const { status, contentType } = await postAndCheckHeaders('/v1/runs', {
      agentId: 'travel',
      messages: [{ role: 'user', content: 'Plan a trip to Paris' }],
    });
    expect(status).toBe(200);
    expect(contentType).toContain('text/event-stream');
  });

  test('research agent accepts run', async () => {
    const { status, contentType } = await postAndCheckHeaders('/v1/runs', {
      agentId: 'research',
      messages: [{ role: 'user', content: 'Research quantum computing' }],
    });
    expect(status).toBe(200);
    expect(contentType).toContain('text/event-stream');
  });

  test('permission agent accepts run', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'permission',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });

  test('stopper agent accepts run', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'stopper',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });

  test('skills agent accepts run', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'skills',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });

  test('a2a agent accepts run', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'a2a',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });

  test('nonexistent agent is rejected or falls back', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'nonexistent_agent_xyz',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    // Server may reject unknown agents (4xx) or fall back to default (200).
    // Either way the response should be well-formed.
    const status = res.status();
    expect(status).toBeGreaterThanOrEqual(200);
    expect(status).toBeLessThan(500);
  });

  test('AG-UI routes to travel agent', async () => {
    const { status, contentType } = await postAndCheckHeaders('/v1/ag-ui/run', {
      agentId: 'travel',
      messages: [{ role: 'user', content: 'Find places in Tokyo' }],
    });
    expect(status).toBe(200);
    expect(contentType).toContain('text/event-stream');
  });

  test('AI SDK routes to research agent', async () => {
    const { status } = await postAndCheckHeaders('/v1/ai-sdk/chat', {
      agentId: 'research',
      messages: aiSdkTextMessages([{ role: 'user', text: 'Search for AI papers' }]),
    });
    expect(status).toBe(200);
  });
});
