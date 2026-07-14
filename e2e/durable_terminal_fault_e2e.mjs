// Durable-path terminal-fault surfacing e2e: the fix that carries `EndCause::Error`
// through `StepOutcome` must hold on BOTH ingress paths. The direct path wires a
// live `StreamSink`; the DURABLE path (AWAKEN_INGRESS=durable) has no live sink and
// degrades to the committed projection — so a failed run there is rendered by
// `encode_step` from the committed `StepOutcome` (terminal = Failed), not by the
// live channel. This pins that the AI-SDK wire still surfaces the error frame when
// the run is settled off-process by the dispatch pool.
//
// Chain: POST /v1/ai-sdk/threads/T/runs -> SharedHost (durable) -> submit_background
//   -> DispatchPool -> Runtime (persistent upstream fault -> retries exhaust ->
//   commit EndCause::Error) -> committed StepOutcome{terminal: Failed} -> encode_step
//   -> `error` + `finish("error")`.
//
// Hermetic (fake upstream, no live key). Run: (from e2e/) node durable_terminal_fault_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { randomBytes } from 'node:crypto';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const FAKE_KEY = 'sk-fake-durable-fault'; // awaken-allow: secret
const PORT = Number(process.env.E2E_PORT ?? 38620);

function frames(raw) {
  const out = [];
  for (const line of raw.split('\n')) {
    const t = line.trim();
    if (!t.startsWith('data:')) continue;
    const p = t.slice(5).trim();
    if (!p || p === '[DONE]') continue;
    try {
      out.push(JSON.parse(p));
    } catch {
      /* ignore */
    }
  }
  return out;
}

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-durable-fault-'));
  const upstream = await startFakeAnthropic(FAKE_KEY, { alwaysFail: true });
  process.env.ANTHROPIC_API_KEY = FAKE_KEY;
  process.env.ANTHROPIC_MODEL = 'fake-haiku';
  process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
  const { server, baseUrl: base } = spawnServer('real', PORT, {
    AWAKEN_INGRESS: 'durable',
    AWAKEN_STORAGE_DIR: dir,
  });
  try {
    await waitForPort(PORT);
    const thread = `dur-fault-${randomBytes(4).toString('hex')}`;

    const res = await fetch(`${base}/v1/ai-sdk/threads/${thread}/runs`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ threadId: thread, messages: [{ id: 'u1', role: 'user', parts: [{ type: 'text', text: 'hello' }] }] }),
    });
    assert.equal(res.status, 200, 'ai-sdk durable run accepted');
    const evs = frames(await res.text());

    // The durable/committed-projection path still surfaces the fault as an error
    // frame — not a silent empty finish.
    const reply = evs.filter((e) => e.type === 'text-delta').map((e) => e.delta).join('');
    assert.ok(!reply.includes('FAKE:'), 'a permanently-failing upstream does not fabricate a reply');
    assert.ok(evs.some((e) => e.type === 'error'), `durable path surfaces an error frame: ${JSON.stringify(evs.map((e) => e.type))}`);
    assert.ok(
      evs.some((e) => e.type === 'finish' && e.finishReason === 'error'),
      `durable path closes with finish("error"): ${JSON.stringify(evs.filter((e) => e.type === 'finish'))}`,
    );
    assert.ok(upstream.attempts >= 2, `retries were attempted before giving up (attempts=${upstream.attempts})`);
    pass('durable ingress: a terminal upstream fault surfaces as an AI-SDK error frame (committed projection, no live sink)');
  } finally {
    await stopServer(server);
    upstream.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }

  console.log('E2E PASS: durable-path terminal-fault surfacing (AI-SDK error frame off the committed projection).');
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
