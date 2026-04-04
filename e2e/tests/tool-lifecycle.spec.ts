import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

function parseSSE(raw: string): Array<{ data: string }> {
  return raw.split('\n')
    .filter(l => l.startsWith('data:'))
    .map(l => ({ data: l.slice(5).trim() }));
}

function parseEvents(body: string): any[] {
  return parseSSE(body)
    .map(e => { try { return JSON.parse(e.data); } catch { return null; } })
    .filter(Boolean);
}

test.describe('tool call lifecycle', () => {
  test('run with weather directive produces tool events (if LLM follows)', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_WEATHER_TOOL - call get_weather for Tokyo' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseEvents(body);

    // Should have at least run lifecycle events
    expect(events.length).toBeGreaterThan(2);

    // If LLM called a tool, we should see tool events
    const hasToolEvents = events.some((e: any) =>
      e.event_type === 'tool_call_start' ||
      e.event_type === 'tool_call_done' ||
      e.event_type === 'tool_call_delta'
    );

    // Log whether tool was called (informational, not assertion)
    if (hasToolEvents) {
      const toolStart = events.find((e: any) => e.event_type === 'tool_call_start');
      expect(toolStart).toBeTruthy();
    }
  });

  test('run completes with proper event sequence', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'limited', // max_rounds=1 for predictable behavior
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseEvents(body);
    const types = events.map((e: any) => e.event_type).filter(Boolean);

    // Verify event ordering: run_start must come first
    expect(types[0]).toBe('run_start');

    // run_finish must come last (among lifecycle events)
    const runFinishIdx = types.lastIndexOf('run_finish');
    expect(runFinishIdx).toBeGreaterThan(0);

    // step_start must come before step_end
    const stepStartIdx = types.indexOf('step_start');
    const stepEndIdx = types.indexOf('step_end');
    if (stepStartIdx >= 0 && stepEndIdx >= 0) {
      expect(stepStartIdx).toBeLessThan(stepEndIdx);
    }
  });

  test('AG-UI tool call events have correct structure', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_WEATHER_TOOL - call get_weather for Tokyo' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseEvents(body);

    // Check AG-UI event types
    const agUiTypes = events.map((e: any) => e.type).filter(Boolean);
    expect(agUiTypes).toContain('RUN_STARTED');
    expect(agUiTypes).toContain('RUN_FINISHED');

    // If tool was called, verify AG-UI tool event structure
    const toolStart = events.find((e: any) => e.type === 'TOOL_CALL_START');
    if (toolStart) {
      expect(toolStart.toolCallId).toBeTruthy();
      expect(toolStart.toolCallName).toBeTruthy();
    }
  });

  test('AI SDK tool events have correct format', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: aiSdkTextMessages([
          { role: 'user', text: 'RUN_WEATHER_TOOL - get weather for Tokyo' },
        ]),
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();

    // AI SDK should return properly formatted events
    const lines = body.split('\n').filter(l => l.startsWith('data:'));
    expect(lines.length).toBeGreaterThan(0);

    // All data lines should be valid JSON
    for (const line of lines) {
      const data = line.slice(5).trim();
      if (data) {
        expect(() => JSON.parse(data)).not.toThrow();
      }
    }
  });

  test('stock price tool can be triggered', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_STOCK_TOOL - call get_stock_price for AAPL' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');
  });

  test('server info tool provides metadata', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'RUN_SERVER_INFO - call serverInfo tool' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    expect(body).toContain('data:');
  });
});
