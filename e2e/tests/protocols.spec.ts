import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';
import { a2aSendMessagePayload } from './a2a-test-utils';

test.describe('AI SDK v6 protocol specifics', () => {
  test('AI SDK chat returns streaming text format', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: aiSdkTextMessages([{ role: 'user', text: 'Tell me something' }]),
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const lines = body.split('\n').filter(l => l.trim().length > 0);
    expect(lines.length).toBeGreaterThan(0);
  });

  test('AI SDK with system message', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: aiSdkTextMessages([
          { role: 'system', text: 'You are helpful' },
          { role: 'user', text: 'Hello' },
        ]),
      },
    });
    expect(res.ok()).toBeTruthy();
  });

  test('AI SDK with thread persistence', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'AI SDK Thread' },
    });
    const thread = await threadRes.json();

    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: aiSdkTextMessages([{ role: 'user', text: 'With thread' }]),
      },
    });
    expect(res.ok()).toBeTruthy();
  });
});

test.describe('AG-UI protocol specifics', () => {
  test('AG-UI run with agent routing', async ({ request }) => {
    const agents = ['default', 'limited'];
    for (const agentId of agents) {
      const res = await request.post('/v1/ag-ui/run', {
        data: {
          agentId,
          messages: [{ role: 'user', content: `Test ${agentId}` }],
        },
      });
      expect(res.ok()).toBeTruthy();
    }
  });

  test('AG-UI events have proper type field', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Check event types' }],
      },
    });
    const body = await res.text();
    const events = body.split('\n')
      .filter(l => l.startsWith('data:'))
      .map(l => {
        try { return JSON.parse(l.slice(5)); } catch { return null; }
      })
      .filter(Boolean);

    const types = events.map((e: any) => e.type).filter(Boolean);
    expect(types.length).toBeGreaterThan(0);
    expect(types).toContain('RUN_STARTED');
  });

  test('AG-UI with thread ID', async ({ request }) => {
    const threadRes = await request.post('/v1/threads', {
      data: { title: 'AG-UI Thread' },
    });
    const thread = await threadRes.json();

    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'With thread' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });
});

test.describe('A2A protocol specifics', () => {
  test('A2A agent card has required v1 fields', async ({ request }) => {
    const res = await request.get('/.well-known/agent-card.json');
    expect(res.ok()).toBeTruthy();

    const card = await res.json();
    expect(card.name).toBeTruthy();
    expect(card.supportedInterfaces?.[0]?.protocolVersion).toBe('1.0');
    expect(card.provider?.url).toBe('http://127.0.0.1:38080');
  });

  test('A2A message:send returns a task wrapper', async ({ request }) => {
    const { taskId, data } = a2aSendMessagePayload('Protocol matrix A2A check');
    const res = await request.post('/v1/a2a/message:send', { data });
    expect(res.ok()).toBeTruthy();

    const body = await res.json();
    expect(body.task?.id).toBe(taskId);
    expect(body.task?.status?.state).toMatch(/^TASK_STATE_/);
  });
});

test.describe('cross-protocol consistency', () => {
  test('same message produces responses from all protocols', async ({ request }) => {
    const msg = [{ role: 'user', content: 'Cross-protocol test' }];
    const aiSdkMessages = aiSdkTextMessages([{ role: 'user', text: 'Cross-protocol test' }]);

    const [runs, agUi, aiSdk] = await Promise.all([
      request.post('/v1/runs', {
        data: { agentId: 'default', messages: msg },
      }),
      request.post('/v1/ag-ui/run', {
        data: { agentId: 'default', messages: msg },
      }),
      request.post('/v1/ai-sdk/chat', {
        data: { agentId: 'default', messages: aiSdkMessages },
      }),
    ]);

    expect(runs.ok()).toBeTruthy();
    expect(agUi.ok()).toBeTruthy();
    expect(aiSdk.ok()).toBeTruthy();

    expect((await runs.text()).length).toBeGreaterThan(0);
    expect((await agUi.text()).length).toBeGreaterThan(0);
    expect((await aiSdk.text()).length).toBeGreaterThan(0);
  });
});
