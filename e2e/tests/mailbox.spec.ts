import { test, expect } from '@playwright/test';

test.describe('mailbox operations', () => {
  test('push message to thread mailbox', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Mailbox Test' },
    });
    const thread = await threadRes.json();

    const pushRes = await request.post(`/v1/threads/${thread.id}/mailbox`, {
      data: { payload: { text: 'Mailbox message' } },
    });
    expect(pushRes.status()).toBe(201);
    const body = await pushRes.json();
    expect(body.job_id).toBeTruthy();
    expect(body.thread_id).toBe(thread.id);
  });

  test('push mailbox with opaque payload', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Opaque Payload Test' },
    });
    const thread = await threadRes.json();

    const pushRes = await request.post(`/v1/threads/${thread.id}/mailbox`, {
      data: { payload: { key: 'value', nested: { a: 1 } } },
    });
    expect(pushRes.status()).toBe(201);
    const body = await pushRes.json();
    expect(body.job_id).toBeTruthy();
  });

  test('peek thread mailbox returns items array', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Peek Test' },
    });
    const thread = await threadRes.json();

    const peekRes = await request.get(`/v1/threads/${thread.id}/mailbox`);
    expect(peekRes.ok()).toBeTruthy();
    const body = await peekRes.json();
    expect(Array.isArray(body.items)).toBeTruthy();
  });

  test('peek mailbox after push shows job', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Peek After Push' },
    });
    const thread = await threadRes.json();

    await request.post(`/v1/threads/${thread.id}/mailbox`, {
      data: { payload: { text: 'queued message' } },
    });

    const peekRes = await request.get(`/v1/threads/${thread.id}/mailbox`);
    expect(peekRes.ok()).toBeTruthy();
    const body = await peekRes.json();
    expect(body.items.length).toBeGreaterThanOrEqual(1);
  });

  test('mailbox on nonexistent thread still accepts push', async ({ request }) => {
    // The mailbox endpoint auto-creates the thread context (lazy creation),
    // so pushing to a nonexistent thread ID succeeds with 201.
    const pushRes = await request.post('/v1/threads/nonexistent-thread-id/mailbox', {
      data: { payload: { text: 'Should still work' } },
    });
    expect(pushRes.status()).toBe(201);
  });
});

test.describe('interrupt and cancellation', () => {
  test('interrupt thread with no active run returns 404', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'Interrupt Test' },
    });
    const thread = await threadRes.json();

    // No run is active, so cancel returns false -> 404
    const interruptRes = await request.post(`/v1/threads/${thread.id}/interrupt`);
    expect(interruptRes.status()).toBe(404);
  });

  test('interrupt nonexistent thread returns 404', async ({ request }) => {
    const res = await request.post('/v1/threads/never-created-thread-id/interrupt');
    expect(res.status()).toBe(404);
  });

  test('cancel nonexistent thread returns 404', async ({ request }) => {
    const res = await request.post('/v1/threads/never-created-thread-id/cancel');
    expect(res.status()).toBe(404);
  });

  test('submit decision to nonexistent thread returns 404', async ({ request }) => {
    const res = await request.post('/v1/threads/never-created-thread-id/decision', {
      data: {
        toolCallId: 'fake-id',
        action: 'resume',
      },
    });
    expect(res.status()).toBe(404);
  });

  test('submit decision with invalid action returns 400', async ({ request }) => {
    const res = await request.post('/v1/threads/nonexistent-thread-id/decision', {
      data: {
        toolCallId: 'fake-id',
        action: 'approve',
      },
    });
    expect(res.status()).toBe(400);
  });
});
