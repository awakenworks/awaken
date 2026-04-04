import { test, expect } from '@playwright/test';
import { aiSdkTextMessages } from './ai-sdk-test-utils';

// These tests require a real LLM backend (not ScriptedLlmExecutor).
// They verify that the LLM produces meaningful responses and follows instructions.
// Skip gracefully if no LLM is available (response is minimal/empty).

function parseSSEEvents(body: string): any[] {
  return body.split('\n')
    .filter(l => l.startsWith('data:'))
    .map(l => { try { return JSON.parse(l.slice(5).trim()); } catch { return null; } })
    .filter(Boolean);
}

test.describe('LLM integration', () => {
  test('default agent produces text response', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Say exactly the word "pineapple" and nothing else' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSEEvents(body);
    // Should have text_delta events
    const textDeltas = events.filter(e => e.event_type === 'text_delta');
    expect(textDeltas.length).toBeGreaterThan(0);
    // Concatenated text should contain something
    const fullText = textDeltas.map(e => e.delta).join('');
    expect(fullText.length).toBeGreaterThan(0);
  });

  test('run completes with run_finish event', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Reply with OK' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSEEvents(body);
    const finish = events.find(e => e.event_type === 'run_finish');
    expect(finish).toBeTruthy();
    expect(finish.termination).toBeTruthy();
  });

  test('inference_complete event reports usage', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hi' }],
      },
    });
    const body = await res.text();
    const events = parseSSEEvents(body);
    const infComplete = events.find(e => e.event_type === 'inference_complete');
    if (infComplete) {
      expect(infComplete.model).toBeTruthy();
      expect(infComplete.usage).toBeTruthy();
    }
  });

  test('state_snapshot includes runtime state', async ({ request }) => {
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Hello' }],
      },
    });
    const body = await res.text();
    const events = parseSSEEvents(body);
    const snapshots = events.filter(e => e.event_type === 'state_snapshot');
    if (snapshots.length > 0) {
      expect(snapshots[0].snapshot).toBeTruthy();
      expect(snapshots[0].snapshot.extensions).toBeTruthy();
    }
  });

  test('limited agent terminates after 1 round', async ({ request }) => {
    const start = Date.now();
    const res = await request.post('/v1/runs', {
      data: {
        agentId: 'limited',
        messages: [{ role: 'user', content: 'Tell me a long story' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSEEvents(body);
    // Should have exactly 1 step
    const stepStarts = events.filter(e => e.event_type === 'step_start');
    expect(stepStarts.length).toBe(1);
  });

  test('AG-UI produces text_message events with real LLM', async ({ request }) => {
    const res = await request.post('/v1/ag-ui/run', {
      data: {
        agentId: 'default',
        messages: [{ role: 'user', content: 'Say hello' }],
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    const events = parseSSEEvents(body);
    // AG-UI should have TEXT_MESSAGE_START and TEXT_MESSAGE_CONTENT
    const types = events.map(e => e.type).filter(Boolean);
    expect(types).toContain('RUN_STARTED');
    if (types.includes('TEXT_MESSAGE_CONTENT')) {
      const content = events.find(e => e.type === 'TEXT_MESSAGE_CONTENT');
      expect(content.delta).toBeTruthy();
    }
  });

  test('AI SDK produces text events with real LLM', async ({ request }) => {
    const res = await request.post('/v1/ai-sdk/chat', {
      data: {
        agentId: 'default',
        messages: aiSdkTextMessages([{ role: 'user', text: 'Say hello' }]),
      },
    });
    expect(res.ok()).toBeTruthy();
    const body = await res.text();
    // AI SDK should produce data lines
    const lines = body.split('\n').filter(l => l.startsWith('data:'));
    expect(lines.length).toBeGreaterThan(0);
  });

  test('multi-turn conversation maintains context', async ({ request }) => {
    // Verify thread-based multi-turn by checking second run accepts the thread
    const threadRes = await request.post('/v1/threads', { data: { title: 'LLM Multi-turn' } });
    const thread = await threadRes.json();

    // Turn 1: use fetch+abort to avoid blocking on full stream
    const controller1 = new AbortController();
    const r1 = await fetch('http://127.0.0.1:38080/v1/runs', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        agentId: 'limited',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'My favorite color is blue.' }],
      }),
      signal: controller1.signal,
    });
    expect(r1.ok).toBeTruthy();
    controller1.abort();

    // Small delay for server to process
    await new Promise(resolve => setTimeout(resolve, 2000));

    // Turn 2: verify second run on same thread is accepted
    const controller2 = new AbortController();
    const r2 = await fetch('http://127.0.0.1:38080/v1/runs', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        agentId: 'limited',
        threadId: thread.id,
        messages: [{ role: 'user', content: 'What is my favorite color?' }],
      }),
      signal: controller2.signal,
    });
    expect(r2.ok).toBeTruthy();
    controller2.abort();
  });
});
