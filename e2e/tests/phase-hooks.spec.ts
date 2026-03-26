import { test, expect } from '@playwright/test';

test.describe('phase hooks', () => {
  test('phases agent runs with phase logger plugin', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'phases',
        messages: [{ role: 'user', content: 'Test phase hooks' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    expect(res.headers()['content-type']).toContain('text/event-stream');
    const body = await res.text();
    expect(body).toContain('data:');
  });

  test('phases agent via AG-UI protocol', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'phases',
        messages: [{ role: 'user', content: 'Phase hooks AG-UI' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body.length).toBeGreaterThan(0);
  });

  test('phases agent via AI SDK protocol', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'phases',
        messages: [{ role: 'user', content: 'Phase hooks AI SDK' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body.length).toBeGreaterThan(0);
  });

  test('phases agent in A2A agent list', async ({ request }) => {
    const res = await request.get('/v1/a2a/agents');
    expect(res.ok()).toBeTruthy();

    const agents = await res.json();
    expect(Array.isArray(agents)).toBeTruthy();
    const ids = agents.map((a: any) => a.agentId);
    expect(ids).toContain('phases');
  });
});
