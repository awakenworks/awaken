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

test.describe('tool execution and SSE streaming', () => {
  test('weather tool request completes with SSE events', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_WEATHER_TOOL' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');

    const events = parseJsonEvents(body);
    // Should produce at least one SSE event
    expect(events.length).toBeGreaterThan(0);
  });

  test('stock tool request completes with SSE events', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_STOCK_TOOL' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');

    const events = parseJsonEvents(body);
    expect(events.length).toBeGreaterThan(0);
  });

  test('normal message returns SSE response', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hello, just chatting' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');

    const events = parseJsonEvents(body);
    expect(events.length).toBeGreaterThan(0);
  });

  test('AG-UI protocol run completes with events', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_WEATHER_TOOL' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();

    const events = parseSSE(body);
    expect(events.length).toBeGreaterThan(0);

    // Verify at least some events are valid JSON
    const parsedData = events
      .filter(e => e.data && e.data.trim())
      .map(e => { try { return JSON.parse(e.data); } catch { return null; } })
      .filter(Boolean);
    expect(parsedData.length).toBeGreaterThan(0);
  });

  test('AI SDK protocol run completes', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'limited',
        messages: aiSdkTextMessages([{ role: 'user', text: 'Say hello' }]),
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body.length).toBeGreaterThan(0);

    const lines = body.split('\n').filter(l => l.trim().length > 0);
    expect(lines.length).toBeGreaterThan(0);
  });
});
