import assert from 'node:assert/strict';
import test from 'node:test';
import { previewOutcome, tail } from './session_preview_driver.mjs';

const start = { type: 'event_start', event: { type: 'agent.message', id: 'message' } };
const delta = (text, id = 'message') => ({
  type: 'event_delta', event_id: id, delta: { type: 'content_delta', content: { type: 'text', text } },
});
const final = { type: 'agent.message', id: 'message', content: [{ type: 'text', text: 'one-two-three' }] };
const idle = { type: 'session.status_idle', id: 'terminal', stop_reason: { type: 'end_turn' } };

test('a missing preview does not change the committed history oracle', () => {
  assert.deepEqual(previewOutcome([final, idle], 'one-two-three'), { count: 0, length: 0 });
  assert.deepEqual(previewOutcome([start, delta('one-'), final, idle], 'one-two-three'), { count: 1, length: 4 });
});

test('rejects a splice, duplicate preview start, foreign response, and late delta', () => {
  for (const frames of [
    [start, delta('one-'), delta('three'), final, idle],
    [start, start, final, idle],
    [delta('one-'), start, final, idle],
    [start, delta('one-', 'other'), final, idle],
    [start, final, delta('one-'), idle],
  ]) assert.throws(() => previewOutcome(frames, 'one-two-three'));
});

test('rejects duplicate or missing durable final events and a changed final payload', () => {
  for (const frames of [[final, final, idle], [idle], [final], [final, idle, idle], [idle, final]]) {
    assert.throws(() => previewOutcome(frames, 'one-two-three'));
  }
  assert.throws(() => previewOutcome([final, idle], 'different'));
  assert.throws(() => previewOutcome([final, { ...idle, stop_reason: { type: 'retries_exhausted' } }], 'one-two-three'));
});

test('reports preview absence separately from observed latency and uses nearest-rank tails', () => {
  assert.deepEqual(tail([null, NaN]), { count: 0, p50: null, p99: null, p999: null });
  const values = Array.from({ length: 1000 }, (_, i) => 1000 - i);
  assert.deepEqual(tail([null, ...values]), { count: 1000, p50: 500, p99: 990, p999: 999 });
});
