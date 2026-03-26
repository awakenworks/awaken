import { test, expect } from '@playwright/test';

test.describe('observability plugin', () => {
  test('run with observability plugin completes without error', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Test observability' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');
  });

  test('multiple runs accumulate metrics without crash', async ({ request }) => {
    for (let i = 0; i < 3; i++) {
      const res = await request.post('/v1/runs', {
        data: {
          agentId: 'default',
          messages: [{ role: 'user', content: `Metrics run ${i}` }],
        },
      });
      expect(res.ok()).toBeTruthy();
    }
  });

  test('observability does not affect other agents', async ({ request }) => {
    // Travel agent doesn't have observability plugin - should still work
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'travel',
        messages: [{ role: 'user', content: 'No observability' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });
});
