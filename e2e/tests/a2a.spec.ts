import { test, expect } from '@playwright/test';
import { a2aSendMessagePayload } from './a2a-test-utils';

test.describe('A2A protocol', () => {
  test('well-known agent card returns latest JSON shape', async ({ request }) => {
    const res = await request.get('/.well-known/agent-card.json');
    expect(res.ok()).toBeTruthy();

    const card = await res.json();
    expect(card.name).toBeTruthy();
    expect(card.supportedInterfaces?.[0]?.url).toContain('/v1/a2a');
    expect(card.supportedInterfaces?.[0]?.protocolBinding).toBe('HTTP+JSON');
    expect(card.supportedInterfaces?.[0]?.protocolVersion).toBe('1.0');
    expect(card.provider?.organization).toBe('Awaken');
    expect(card.provider?.url).toMatch(/^http:\/\/127\.0\.0\.1:38080$/);
    expect(card.capabilities?.streaming).toBe(true);
    expect(card.capabilities?.pushNotifications).toBe(true);
    expect(card.capabilities?.extendedAgentCard).toBe(false);
    expect(card.url).toBeUndefined();
  });

  test('message:send returns task wrapper and task is retrievable', async ({ request }) => {
    const { taskId, data } = a2aSendMessagePayload('Hello via A2A');
    const sendRes = await request.post('/v1/a2a/message:send', { data });
    expect(sendRes.ok()).toBeTruthy();

    const body = await sendRes.json();
    expect(body.task?.id).toBe(taskId);
    expect(body.task?.contextId).toBe(taskId);
    expect(body.task?.status?.state).toMatch(/^TASK_STATE_/);

    const taskRes = await request.get(`/v1/a2a/tasks/${taskId}?historyLength=10`);
    expect(taskRes.ok()).toBeTruthy();
    const task = await taskRes.json();
    expect(task.id).toBe(taskId);
    expect(task.contextId).toBe(taskId);
    expect(task.status?.state).toMatch(/^TASK_STATE_/);
  });

  test('tenant-scoped message:send works', async ({ request }) => {
    const { taskId, data } = a2aSendMessagePayload('Hello limited agent');
    const sendRes = await request.post('/v1/a2a/limited/message:send', { data });
    expect(sendRes.ok()).toBeTruthy();

    const body = await sendRes.json();
    expect(body.task?.id).toBe(taskId);

    const taskRes = await request.get(`/v1/a2a/limited/tasks/${taskId}`);
    expect(taskRes.ok()).toBeTruthy();
    const task = await taskRes.json();
    expect(task.id).toBe(taskId);
    expect(task.contextId).toBe(taskId);
  });

  test('message:stream returns SSE updates', async ({ request }) => {
    const { data } = a2aSendMessagePayload('Hello stream');
    const res = await request.post('/v1/a2a/message:stream', {
      headers: { 'content-type': 'application/a2a+json' },
      data,
    });
    expect(res.ok()).toBeTruthy();
    expect(res.headers()['content-type']).toContain('text/event-stream');

    const body = await res.text();
    expect(body).toContain('"task"');
    expect(body).toContain('TASK_STATE_');
  });

  test('push notification config CRUD works', async ({ request }) => {
    const { taskId, data } = a2aSendMessagePayload('Hello push configs');
    const sendRes = await request.post('/v1/a2a/message:send', { data });
    expect(sendRes.ok()).toBeTruthy();

    const createRes = await request.post(`/v1/a2a/tasks/${taskId}/pushNotificationConfigs`, {
      data: {
        url: 'https://example.com/webhook',
        token: 'push-token',
      },
    });
    expect(createRes.ok()).toBeTruthy();
    const created = await createRes.json();
    expect(created.taskId).toBe(taskId);
    expect(created.id).toBeTruthy();

    const listRes = await request.get(`/v1/a2a/tasks/${taskId}/pushNotificationConfigs`);
    expect(listRes.ok()).toBeTruthy();
    const listed = await listRes.json();
    expect(Array.isArray(listed.configs)).toBe(true);
    expect(listed.configs[0]?.id).toBe(created.id);

    const getRes = await request.get(
      `/v1/a2a/tasks/${taskId}/pushNotificationConfigs/${created.id}`,
    );
    expect(getRes.ok()).toBeTruthy();

    const deleteRes = await request.delete(
      `/v1/a2a/tasks/${taskId}/pushNotificationConfigs/${created.id}`,
    );
    expect(deleteRes.status()).toBe(204);
  });

  test('unsupported version is rejected', async ({ request }) => {
    const res = await request.get('/.well-known/agent-card.json', {
      headers: {
        'a2a-version': '0.9',
      },
    });
    expect(res.status()).toBe(400);

    const body = await res.json();
    expect(body.error?.details?.[0]?.reason).toBe('VERSION_NOT_SUPPORTED');
  });

  test('invalid inbound role is rejected', async ({ request }) => {
    const res = await request.post('/v1/a2a/message:send', {
      data: {
        message: {
          taskId: 'invalid-role-task',
          contextId: 'invalid-role-task',
          messageId: 'msg-invalid-role',
          role: 'ROLE_AGENT',
          parts: [{ text: 'hello' }],
        },
      },
    });
    expect(res.status()).toBe(400);

    const body = await res.json();
    expect(body.error?.status).toBe('INVALID_ARGUMENT');
    expect(body.error?.details?.[0]?.fieldViolations?.[0]?.field).toBe('message.role');
  });
});
