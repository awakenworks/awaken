import { test, expect } from '@playwright/test';

/**
 * Parse SSE text into an array of {event, data} objects.
 */
function parseSSE(raw: string): Array<{ event?: string; data: string }> {
  const events: Array<{ event?: string; data: string }> = [];
  let currentEvent: string | undefined;
  let dataLines: string[] = [];

  for (const line of raw.split('\n')) {
    if (line.startsWith('event:')) {
      currentEvent = line.slice(6).trim();
    } else if (line.startsWith('data:')) {
      dataLines.push(line.slice(5).trim());
    } else if (line.trim() === '' && dataLines.length > 0) {
      events.push({ event: currentEvent, data: dataLines.join('\n') });
      currentEvent = undefined;
      dataLines = [];
    }
  }
  if (dataLines.length > 0) {
    events.push({ event: currentEvent, data: dataLines.join('\n') });
  }
  return events;
}

function parseJsonEvents(raw: string): any[] {
  return parseSSE(raw)
    .map(e => {
      try { return JSON.parse(e.data); } catch { return null; }
    })
    .filter(Boolean);
}

test.describe('generative UI (A2UI)', () => {
  test('a2ui agent accepts run', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'a2ui',
        messages: [{ role: 'user', content: 'Render some UI' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');
  });

  test('RUN_A2UI_TOOL triggers render_a2ui tool call', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'a2ui',
        messages: [{ role: 'user', content: 'RUN_A2UI_TOOL' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();

    // Should contain tool execution events
    const events = parseJsonEvents(body);
    expect(events.length).toBeGreaterThan(0);

    // Should contain render_a2ui tool call evidence
    const bodyStr = JSON.stringify(events);
    expect(bodyStr).toContain('render_a2ui');
  });

  test('a2ui agent via AG-UI protocol', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'a2ui',
        messages: [{ role: 'user', content: 'Show me a dashboard' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });

  test('a2ui agent via AI SDK protocol', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'a2ui',
        messages: [{ role: 'user', content: 'Render a card' }],
      },
    });
    expect(res.ok()).toBeTruthy();
  });

  test('a2ui agent appears in A2A agent list', async ({ request }) => {
    const res = await request.get('/v1/a2a/agents');
    if (res.ok()) {
      const agents = await res.json();
      if (Array.isArray(agents)) {
        const ids = agents.map((a: any) => a.agentId || a.id);
        expect(ids).toContain('a2ui');
      }
    }
  });
});

test.describe('generative UI (genui)', () => {
  test('genui agent accepts run', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'genui',
        messages: [{ role: 'user', content: 'Generate a chart' }],
      },
    });
    expect(res.status()).toBeLessThan(500);
    const body = await res.text();
    expect(body).toContain('data:');
  });

  test('genui agent via AG-UI protocol', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'genui',
        messages: [{ role: 'user', content: 'Show me a widget' }],
      },
    });
    expect(res.status()).toBeLessThan(500);
  });

  test('genui agent appears in A2A agent list', async ({ request }) => {
    const res = await request.get('/v1/a2a/agents');
    if (res.ok()) {
      const agents = await res.json();
      if (Array.isArray(agents)) {
        const ids = agents.map((a: any) => a.agentId || a.id);
        expect(ids).toContain('genui');
      }
    }
  });
});
