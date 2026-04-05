import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';
import { a2aSendMessagePayload } from './a2a-test-utils';

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

test.describe('phase hooks', () => {
  test('phases agent runs with phase logger plugin', async () => {
    const { status, contentType } = await postAndCheckHeaders('/v1/runs', {
      agentId: 'phases',
      messages: [{ role: 'user', content: 'Test phase hooks' }],
    });
    expect(status).toBe(200);
    expect(contentType).toContain('text/event-stream');
  });

  test('phases agent via AG-UI protocol', async () => {
    const { status, contentType } = await postAndCheckHeaders('/v1/ag-ui/run', {
      agentId: 'phases',
      messages: [{ role: 'user', content: 'Phase hooks AG-UI' }],
    });
    expect(status).toBe(200);
    expect(contentType).toContain('text/event-stream');
  });

  test('phases agent via AI SDK protocol', async () => {
    const { status } = await postAndCheckHeaders('/v1/ai-sdk/chat', {
      agentId: 'phases',
      messages: aiSdkTextMessages([{ role: 'user', text: 'Phase hooks AI SDK' }]),
    });
    expect(status).toBe(200);
  });

  test('phases agent accepts A2A tenant route', async ({ request }) => {
    const { taskId, data } = a2aSendMessagePayload('Phase hooks via A2A');
    const res = await request.post('/v1/a2a/phases/message:send', { data });
    expect(res.ok()).toBeTruthy();

    const body = await res.json();
    expect(body.task?.id).toBe(taskId);
  });
});
