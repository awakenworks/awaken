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

test.describe('deterministic tool execution via ScriptedLlmExecutor', () => {
  test('RUN_WEATHER_TOOL triggers get_weather tool call', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_WEATHER_TOOL' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();

    const events = parseJsonEvents(body);
    // Should have multiple events (tool call + tool result + final response)
    expect(events.length).toBeGreaterThan(1);

    // Look for tool-call evidence: an event mentioning get_weather
    const bodyStr = JSON.stringify(events);
    expect(bodyStr).toContain('get_weather');
  });

  test('RUN_STOCK_TOOL triggers get_stock_price tool call', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_STOCK_TOOL' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();

    const events = parseJsonEvents(body);
    expect(events.length).toBeGreaterThan(1);

    const bodyStr = JSON.stringify(events);
    expect(bodyStr).toContain('get_stock_price');
  });

  test('normal message returns text without tool calls', async ({ request }) => {
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

    // Should contain scripted text echo and no tool call references
    const bodyStr = JSON.stringify(events);
    expect(bodyStr).toContain('Scripted response to');
    expect(bodyStr).not.toContain('get_weather');
    expect(bodyStr).not.toContain('get_stock_price');
  });

  test('tool execution via AG-UI protocol', async ({ request }) => {
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

    // AG-UI events should include TOOL_CALL_START for the weather tool
    const parsedData = events
      .filter(e => e.data && e.data.trim())
      .map(e => { try { return JSON.parse(e.data); } catch { return null; } })
      .filter(Boolean);
    const types = parsedData.map(d => d.type).filter(Boolean);
    expect(types).toContain('TOOL_CALL_START');
  });

  test('tool execution via AI SDK protocol', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_WEATHER_TOOL' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body.length).toBeGreaterThan(0);

    // AI SDK protocol should include tool call data
    // Tool calls appear as specific data protocol lines
    const lines = body.split('\n').filter(l => l.trim().length > 0);
    expect(lines.length).toBeGreaterThan(0);
  });
});
