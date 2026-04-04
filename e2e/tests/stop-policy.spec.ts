import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

test.describe('stop policies', () => {
  test('limited agent (max_rounds=1) terminates quickly', async ({ request }) => {
    const start = Date.now();
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'limited',
        messages: [{ role: 'user', content: 'Hello limited agent' }],
      },
    });
    const elapsed = Date.now() - start;
    expect(res.ok()).toBeTruthy();

    const body = await res.text();
    expect(body).toContain('data:');
    // Limited agent should respond quickly (1 round only)
    expect(elapsed).toBeLessThan(30_000);
  });

  test('limited agent works via AG-UI', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'limited',
        messages: [{ role: 'user', content: 'Quick response please' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });

  test('limited agent works via AI SDK', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'limited',
        messages: aiSdkTextMessages([{ role: 'user', text: 'Quick AI SDK response' }]),
      },
    });
    expect(res.ok()).toBeTruthy();
  });

  test('default agent (max_rounds=8) also terminates', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hello default agent' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });
});
