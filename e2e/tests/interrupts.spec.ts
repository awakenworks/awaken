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

test.describe('AG-UI interrupts and RUN_FINISHED', () => {
  test('normal run has no interrupt in RUN_FINISHED', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseJsonEvents(body);

    const runFinished = events.find(e => e.type === 'RUN_FINISHED');
    expect(runFinished).toBeDefined();
    // Normal run should not have interrupt or outcome=interrupt
    expect(runFinished.outcome).toBeUndefined();
    expect(runFinished.interrupt).toBeUndefined();
  });

  test('RUN_FINISHED event has threadId and runId', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hi' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseJsonEvents(body);

    const runFinished = events.find(e => e.type === 'RUN_FINISHED');
    expect(runFinished).toBeDefined();
    expect(typeof runFinished.threadId).toBe('string');
    expect(runFinished.threadId.length).toBeGreaterThan(0);
    expect(typeof runFinished.runId).toBe('string');
    expect(runFinished.runId.length).toBeGreaterThan(0);
  });

  test('resume field in request is accepted', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Continue' }],
        resume: {
          interruptId: 'int-1',
          payload: { approved: true },
        },
      },
    });
    // Should not crash; the resume is accepted even if no actual suspension
    expect(res.status()).toBeLessThan(500);
  });
});
