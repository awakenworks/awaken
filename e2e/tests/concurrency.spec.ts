import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

test.describe('concurrent operations', () => {
  test('multiple runs on same thread execute without server error', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Concurrent Test' },
    });
    const thread = await threadRes.json();

    // Fire two runs concurrently on the same thread
    const [run1, run2] = await Promise.all([
      request.post('/v1/runs', {
        data: {
          agentId: 'default',
          threadId: thread.id,
          messages: [{ role: 'user', content: 'First message' }],
        },
      }),
      request.post('/v1/runs', {
        data: {
          agentId: 'default',
          threadId: thread.id,
          messages: [{ role: 'user', content: 'Second message' }],
        },
      }),
    ]);

    // Both should complete without server error.
    // One may succeed (200) while the other gets queued or rejected (409).
    expect(run1.status()).toBeLessThan(500);
    expect(run2.status()).toBeLessThan(500);
  });

  test('runs on different threads are isolated', async ({ request }) => {
    const [t1, t2] = await Promise.all([
      request.post('/v1/threads', { data: { title: 'Thread A' } }),
      request.post('/v1/threads', { data: { title: 'Thread B' } }),
    ]);
    const thread1 = await t1.json();
    const thread2 = await t2.json();

    const [r1, r2] = await Promise.all([
      request.post('/v1/runs', {
        data: {
          agentId: 'default',
          threadId: thread1.id,
          messages: [{ role: 'user', content: 'Hello Thread A' }],
        },
      }),
      request.post('/v1/runs', {
        data: {
          agentId: 'limited',
          threadId: thread2.id,
          messages: [{ role: 'user', content: 'Hello Thread B' }],
        },
      }),
    ]);

    expect(r1.ok()).toBeTruthy();
    expect(r2.ok()).toBeTruthy();
  });

  test('rapid thread creation does not conflict', async ({ request }) => {
    const promises = Array.from({ length: 5 }, (_, i) =>
      request.post('/v1/threads', { data: { title: `Rapid ${i}` } })
    );
    const results = await Promise.all(promises);

    const ids = new Set<string>();
    for (const res of results) {
      expect(res.ok()).toBeTruthy();
      const body = await res.json();
      expect(body.id).toBeTruthy();
      ids.add(body.id);
    }
    // All IDs should be unique
    expect(ids.size).toBe(5);
  });

  test('different agents on different protocols concurrently', async ({ request }) => {
    const [agUi, aiSdk, runs] = await Promise.all([
      request.post('/v1/ag-ui/run', {
        data: {
          agentId: 'default',
          messages: [{ role: 'user', content: 'AG-UI concurrent' }],
        },
      }),
      request.post('/v1/ai-sdk/chat', {
        data: {
          agentId: 'limited',
          messages: aiSdkTextMessages([{ role: 'user', text: 'AI SDK concurrent' }]),
        },
      }),
      request.post('/v1/runs', {
        data: {
          agentId: 'limited',
          messages: [{ role: 'user', content: 'Runs API concurrent' }],
        },
      }),
    ]);

    expect(agUi.ok()).toBeTruthy();
    expect(aiSdk.ok()).toBeTruthy();
    expect(runs.ok()).toBeTruthy();
  });
});
