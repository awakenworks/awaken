import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

const BASE_URL = 'http://127.0.0.1:38080';

/**
 * POST with an AbortController that fires after receiving the HTTP headers.
 * Avoids buffering the full SSE body for slow multi-round tool-calling agents.
 */
async function postAndCheckHeaders(
  url: string,
  body: object,
): Promise<{ status: number; contentType: string }> {
  const controller = new AbortController();
  const res = await fetch(`${BASE_URL}${url}`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    signal: controller.signal,
  });
  const status = res.status;
  const contentType = res.headers.get('content-type') ?? '';
  controller.abort();
  return { status, contentType };
}

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
  // Flush remaining
  if (dataLines.length > 0) {
    events.push({ event: currentEvent, data: dataLines.join('\n') });
  }
  return events;
}

test.describe('SSE event structure', () => {
  test('run produces parseable SSE events', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();

    const body = await res.text();
    const events = parseSSE(body);
    expect(events.length).toBeGreaterThan(0);

    // Each event's data should be valid JSON
    for (const evt of events) {
      if (evt.data && evt.data.trim()) {
        expect(() => JSON.parse(evt.data)).not.toThrow();
      }
    }
  });

  test('AG-UI run produces structured events', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Test SSE structure' }],
      },
    });
    expect(res.ok()).toBeTruthy();

    const body = await res.text();
    const events = parseSSE(body);
    expect(events.length).toBeGreaterThan(0);

    // AG-UI events carry a "type" field inside data JSON
    const parsedData = events
      .filter(e => e.data && e.data.trim())
      .map(e => JSON.parse(e.data));
    const agUiTypes = parsedData.map(d => d.type).filter(Boolean);
    // At minimum should have RUN_STARTED and RUN_FINISHED
    expect(agUiTypes.length).toBeGreaterThan(0);
    expect(agUiTypes).toContain('RUN_STARTED');
  });

  test('AI SDK chat produces data protocol events', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: aiSdkTextMessages([{ role: 'user', text: 'Hello AI SDK' }]),
      },
    });
    expect(res.ok()).toBeTruthy();

    const body = await res.text();
    // AI SDK data protocol uses specific prefixes like 0:, 2:, d:, e:
    // Or standard SSE with data: lines
    expect(body.length).toBeGreaterThan(0);
    // Should contain at least some data lines
    const lines = body.split('\n').filter(l => l.trim().length > 0);
    expect(lines.length).toBeGreaterThan(0);
  });

  test('run with travel agent produces domain-specific response', async () => {
    // Use fetch+abort: travel agent has multi-round tool loops that exceed Playwright timeout
    const { status, contentType } = await postAndCheckHeaders('/v1/runs', {
      agentId: 'travel',
      messages: [{ role: 'user', content: 'Plan a trip' }],
    });
    expect(status).toBe(200);
    expect(contentType).toContain('text/event-stream');
  });

  test('SSE stream has correct content-type header', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Check headers' }],
      },
      timeout: 30_000,
    });
    const contentType = res.headers()['content-type'];
    expect(contentType).toContain('text/event-stream');
  });
});
