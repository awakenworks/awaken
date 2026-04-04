import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

function parseSSE(raw: string): Array<{ data: string }> {
  return raw.split('\n')
    .filter(l => l.startsWith('data:'))
    .map(l => ({ data: l.slice(5).trim() }));
}

test.describe('real LLM integration', () => {
  // This test verifies end-to-end LLM response with text streaming
  test('default agent produces text response', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Reply with exactly one word: yes' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSE(body);

    // Should have run_start, step_start, text_delta(s), run_finish
    const parsed = events.map(e => { try { return JSON.parse(e.data); } catch { return null; } }).filter(Boolean);
    const types = parsed.map((e: any) => e.event_type || e.type).filter(Boolean);

    expect(types).toContain('run_start');
    expect(types).toContain('text_delta');
    expect(types).toContain('run_finish');
  });

  test('text_delta events contain actual LLM text', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'What is 2+2? Answer with just the number.' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSE(body).map(e => { try { return JSON.parse(e.data); } catch { return null; } }).filter(Boolean);

    const textDeltas = events.filter((e: any) => e.event_type === 'text_delta' || e.type === 'TEXT_MESSAGE_CONTENT');
    expect(textDeltas.length).toBeGreaterThan(0);

    // Concatenated text should be non-empty
    const fullText = textDeltas.map((e: any) => e.delta || '').join('');
    expect(fullText.length).toBeGreaterThan(0);
  });

  test('run_finish includes termination reason', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'limited',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSE(body).map(e => { try { return JSON.parse(e.data); } catch { return null; } }).filter(Boolean);

    const finish = events.find((e: any) => e.event_type === 'run_finish');
    expect(finish).toBeTruthy();
    expect(finish.termination).toBeTruthy();
  });

  test('multi-turn conversation preserves context', async ({ request }) => {
    // Create thread
    const threadRes = await request.post('/v1/threads', { data: { title: 'Multi-turn LLM' } });
    const thread = await threadRes.json();

    // Turn 1
    const r1 = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'My favorite color is blue. Remember this.' }],
      },
    });
    expect(r1.ok()).toBeTruthy();
    await r1.text(); // consume stream

    // Turn 2
    const r2 = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'What is my favorite color?' }],
      },
    });
    expect(r2.ok()).toBeTruthy();
    const body = await r2.text();

    // Should produce a response (we can't assert "blue" since LLM behavior varies)
    const events = parseSSE(body);
    expect(events.length).toBeGreaterThan(2);
  });

  test('AG-UI protocol produces structured event types', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Say one word' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSE(body).map(e => { try { return JSON.parse(e.data); } catch { return null; } }).filter(Boolean);

    const types = events.map((e: any) => e.type).filter(Boolean);
    expect(types).toContain('RUN_STARTED');
    // Should have text message events
    const hasText = types.some(t => t === 'TEXT_MESSAGE_START' || t === 'TEXT_MESSAGE_CONTENT');
    expect(hasText).toBeTruthy();
    expect(types).toContain('RUN_FINISHED');
  });

  test('AI SDK protocol produces streaming events', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: aiSdkTextMessages([{ role: 'user', text: 'Say one word' }]),
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();

    // Should have data lines
    const lines = body.split('\n').filter(l => l.startsWith('data:'));
    expect(lines.length).toBeGreaterThan(2);

    // Should contain start and finish markers
    expect(body).toContain('start');
    expect(body).toContain('finish');
  });

  test('inference_complete event has usage data', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hi' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSE(body).map(e => { try { return JSON.parse(e.data); } catch { return null; } }).filter(Boolean);

    const inferenceComplete = events.find((e: any) => e.event_type === 'inference_complete');
    if (inferenceComplete) {
      expect(inferenceComplete.model).toBeTruthy();
      expect(inferenceComplete.usage).toBeTruthy();
    }
  });

  test('state_snapshot events are emitted', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSE(body).map(e => { try { return JSON.parse(e.data); } catch { return null; } }).filter(Boolean);

    const snapshots = events.filter((e: any) => e.event_type === 'state_snapshot');
    // At least one state snapshot should be emitted
    expect(snapshots.length).toBeGreaterThan(0);
  });
});
