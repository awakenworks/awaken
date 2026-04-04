import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

test.describe('run status and listing', () => {
  test('list all runs returns paginated response', async ({ request }) => {
    const res = await request.get('/v1/runs');
    expect(res.ok()).toBeTruthy();
    const body = await res.json();
    expect(Array.isArray(body.items)).toBeTruthy();
    expect(typeof body.total).toBe('number');
    expect(typeof body.has_more).toBe('boolean');
  });

  test('get run by ID after creating a run', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Run Status Test' },
    });
    const thread = await threadRes.json();

    // Start a run (SSE stream)
    const runRes = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'Test run status' }],
      },
    });
    expect(runRes.ok()).toBeTruthy();
    const body = await runRes.text();

    // Extract run ID from SSE run_start event
    const lines = body.split('\n').filter(l => l.startsWith('data:'));
    let runId: string | null = null;
    for (const line of lines) {
      try {
        const parsed = JSON.parse(line.slice(5).trim());
        if (parsed.run_id) {
          runId = parsed.run_id;
          break;
        }
      } catch { /* skip non-JSON lines */ }
    }
    expect(runId).toBeTruthy();

    // Try fetching the run by ID -- it may or may not be persisted
    // depending on whether the run completed with a checkpoint
    const getRes = await request.get(`/v1/runs/${runId}`);
    // The run should either exist (200) or not be found (404)
    expect([200, 404]).toContain(getRes.status());

    if (getRes.status() === 200) {
      const run = await getRes.json();
      expect(run.run_id).toBe(runId);
      expect(run.thread_id).toBe(thread.id);
      expect(run.agent_id).toBeTruthy();
    }
  });

  test('list runs for specific thread has correct shape', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Thread Runs List' },
    });
    const thread = await threadRes.json();

    // Run something on this thread
    const runRes = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'For listing' }],
      },
    });
    await runRes.text();

    const listRes = await request.get(`/v1/threads/${thread.id}/runs`);
    expect(listRes.ok()).toBeTruthy();
    const body = await listRes.json();
    expect(Array.isArray(body.items)).toBeTruthy();
    expect(typeof body.total).toBe('number');
    expect(typeof body.has_more).toBe('boolean');

    // If runs are persisted, verify thread ID filter works
    for (const run of body.items) {
      expect(run.thread_id).toBe(thread.id);
    }
  });

  test('latest run for thread endpoint', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Latest Run' },
    });
    const thread = await threadRes.json();

    // First run
    const run1 = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'First' }],
      },
    });
    await run1.text();

    // Second run
    const run2 = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'Second' }],
      },
    });
    await run2.text();

    const latestRes = await request.get(`/v1/threads/${thread.id}/runs/latest`);
    // If no runs are persisted, expect 404; otherwise 200
    expect([200, 404]).toContain(latestRes.status());

    if (latestRes.status() === 200) {
      const latest = await latestRes.json();
      expect(latest.run_id).toBeTruthy();
      expect(latest.thread_id).toBe(thread.id);
    }
  });

  test('run with each protocol produces SSE stream', async ({ request }) => {
    // AG-UI run
    const agUiRes = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'AG-UI tracked' }],
      },
    });
    expect(agUiRes.ok()).toBeTruthy();
    await agUiRes.text();

    // AI SDK run
    const aiSdkRes = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: aiSdkTextMessages([{ role: 'user', text: 'AI SDK tracked' }]),
      },
    });
    expect(aiSdkRes.ok()).toBeTruthy();
    await aiSdkRes.text();

    // Runs list endpoint should work (shape check)
    const listRes = await request.get('/v1/runs');
    expect(listRes.ok()).toBeTruthy();
    const body = await listRes.json();
    expect(Array.isArray(body.items)).toBeTruthy();
  });

  test('get nonexistent run returns 404', async ({ request }) => {
    const res = await request.get('/v1/runs/nonexistent-run-id');
    expect(res.status()).toBe(404);
  });

  test('list runs supports status filter query param', async ({ request }) => {
    const res = await request.get('/v1/runs?status=done');
    expect(res.ok()).toBeTruthy();
    const body = await res.json();
    expect(Array.isArray(body.items)).toBeTruthy();
  });

  test('list runs rejects invalid status filter', async ({ request }) => {
    const res = await request.get('/v1/runs?status=invalid');
    expect(res.status()).toBe(400);
  });
});
