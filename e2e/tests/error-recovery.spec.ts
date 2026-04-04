import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

test.describe('error recovery and edge cases', () => {
  test('nonexistent agent returns error without crashing server', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'nonexistent_xyz_agent',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    // Should get an error response, not 500
    expect(res.status()).toBeLessThan(500);
    // Server should still be healthy after error
    const health = await request.get('/health');
    expect(health.ok()).toBeTruthy();
  });

  test('empty message content is handled gracefully', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: '' }],
      },
    });
    expect(res.status()).toBeLessThan(500);
  });

  test('very long message is accepted', async ({ request }) => {
    const longText = 'A'.repeat(10000);
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'limited',
        messages: [{ role: 'user', content: longText }],
      },
    });
    // Should not crash — may return 200 (processed) or 4xx (rejected)
    expect(res.status()).toBeLessThan(500);
  });

  test('concurrent requests to same thread are handled', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', { data: { title: 'Concurrent Error' } });
    const thread = await threadRes.json();

    // Fire 3 concurrent requests to same thread
    const results = await Promise.all([
      request.post('/v1/runs', {
        data: { agentId: 'limited', threadId: thread.id, messages: [{ role: 'user', content: 'One' }] },
      }),
      request.post('/v1/runs', {
        data: { agentId: 'limited', threadId: thread.id, messages: [{ role: 'user', content: 'Two' }] },
      }),
      request.post('/v1/runs', {
        data: { agentId: 'limited', threadId: thread.id, messages: [{ role: 'user', content: 'Three' }] },
      }),
    ]);

    // All should complete without 5xx
    for (const res of results) {
      expect(res.status()).toBeLessThan(500);
    }
  });

  test('server recovers after bad request', async ({ request }) => {
    // Send malformed JSON
    const badRes = await request.post('/v1/runs', {
      headers: { 'content-type': 'application/json' },
      data: '{invalid json',
    });
    expect(badRes.status()).toBeGreaterThanOrEqual(400);

    // Server should still work
    const goodRes = await request.post('/v1/runs', {
      data: {
        agentId: 'limited',
        messages: [{ role: 'user', content: 'Recovery test' }],
      },
    });
    expect(goodRes.ok()).toBeTruthy();
  });

  test('thread operations work after failed run', async ({ request }) => {
    // Create thread
    const threadRes = await request.post('/v1/threads', { data: { title: 'Recovery Thread' } });
    const thread = await threadRes.json();

    // Trigger a run with nonexistent agent (should fail)
    await request.post('/v1/runs', {
      data: {
        agentId: 'nonexistent',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'Will fail' }],
      },
    });

    // Thread should still be accessible
    const getRes = await request.get(`/v1/threads/${thread.id}`);
    expect(getRes.ok()).toBeTruthy();
  });

  test('rapid successive requests to health endpoint', async ({ request }) => {
    const results = await Promise.all(
      Array.from({ length: 20 }, () => request.get('/health'))
    );
    for (const res of results) {
      expect(res.ok()).toBeTruthy();
    }
  });

  test('multiple protocols concurrently without error', async ({ request }) => {
    const msg = [{ role: 'user', content: 'Quick' }];
    const aiSdkMessages = aiSdkTextMessages([{ role: 'user', text: 'Quick' }]);
    const [r1, r2, r3] = await Promise.all([
      request.post('/v1/runs', { data: { agentId: 'limited', messages: msg } }),
      request.post('/v1/ag-ui/run', { data: { agentId: 'limited', messages: msg } }),
      request.post('/v1/ai-sdk/chat', { data: { agentId: 'limited', messages: aiSdkMessages } }),
    ]);
    expect(r1.status()).toBeLessThan(500);
    expect(r2.status()).toBeLessThan(500);
    expect(r3.status()).toBeLessThan(500);
  });
});
