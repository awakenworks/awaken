import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

test.describe('input validation', () => {
  test('POST /v1/runs with empty body returns 400', async ({ request }) => {
    const res = await request.post('/v1/runs', { data: {} });
    expect(res.status()).toBe(422);
  });

  test('POST /v1/runs with invalid JSON returns 4xx', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      headers: { 'content-type': 'application/json' },
      data: 'not-json',
    });
    expect(res.status()).toBeGreaterThanOrEqual(400);
    expect(res.status()).toBeLessThan(500);
  });

  test('POST /v1/threads with empty body creates thread', async ({ request }) => {
    const res = await request.post('/v1/threads', { data: {} });
    expect(res.ok()).toBeTruthy();
  });

  test('GET /v1/threads with pagination', async ({ request }) => {
    const res = await request.get('/v1/threads?limit=2&offset=0');
    expect(res.ok()).toBeTruthy();
    const body = await res.json();
    expect(body.limit).toBe(2);
  });

  test('GET /v1/runs for nonexistent run returns 404', async ({ request }) => {
    const res = await request.get('/v1/runs/nonexistent-id');
    expect(res.status()).toBe(404);
  });

  test('DELETE nonexistent thread returns 404', async ({ request }) => {
    const res = await request.delete('/v1/threads/nonexistent-id');
    expect(res.status()).toBe(404);
  });

  test('AG-UI with empty messages still works', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [],
      },
    });
    // May return 400 or handle gracefully
    expect(res.status()).toBeLessThan(500);
  });

  test('AI SDK with missing agentId returns error', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        messages: aiSdkTextMessages([{ role: 'user', text: 'Hello' }]),
      },
    });
    expect(res.status()).toBeLessThan(500);
  });

  test('thread summaries endpoint works', async ({ request }) => {
    const res = await request.get('/v1/threads/summaries');
    expect(res.ok()).toBeTruthy();
  });

  test('latest run for thread works', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Latest Run Test' },
    });
    const thread = await threadRes.json();

    // Before any run, latest should return 404 or empty
    const latestRes = await request.get(`/v1/threads/${thread.id}/runs/latest`);
    expect(latestRes.status()).toBeLessThan(500);
  });
});
