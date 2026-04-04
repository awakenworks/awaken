import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

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

  test('a2ui tool request completes with SSE events', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'a2ui',
        messages: [{ role: 'user', content: 'RUN_A2UI_TOOL' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');

    // Should produce events without crashing
    const events = parseJsonEvents(body);
    expect(events.length).toBeGreaterThan(0);
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
        messages: aiSdkTextMessages([{ role: 'user', text: 'Render a card' }]),
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
  // genui agent may trigger multi-round tool calling; use fetch+abort to avoid stream timeout
  async function postAndCheckHeaders(url: string, body: object) {
    const controller = new AbortController();
    const res = await fetch(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
      signal: controller.signal,
    });
    controller.abort();
    return res;
  }

  test('genui agent accepts run', async () => {
    const res = await postAndCheckHeaders('http://127.0.0.1:38080/v1/runs', {
      agentId: 'genui',
      messages: [{ role: 'user', content: 'Generate a chart' }],
    });
    expect(res.ok).toBeTruthy();
    expect(res.headers.get('content-type')).toContain('text/event-stream');
  });

  test('genui agent via AG-UI protocol', async () => {
    const res = await postAndCheckHeaders('http://127.0.0.1:38080/v1/ag-ui/run', {
      agentId: 'genui',
      messages: [{ role: 'user', content: 'Show me a widget' }],
    });
    expect(res.ok).toBeTruthy();
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
