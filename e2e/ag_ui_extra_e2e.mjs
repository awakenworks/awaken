// AG-UI adapter — the paths the `HttpAgent` happy-path e2e does not reach: the
// non-scoped `/v1/ag-ui` run route (plain-string message content), and the
// malformed-body error path (the AgUiJson extractor fails closed with an AG-UI
// error event, not a crash). Driven with raw HTTP; the SSE stream is read as text.
//
// Run: (from e2e/)  node ag_ui_extra_e2e.mjs

import assert from 'node:assert/strict';
import { spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38187);
const BASE = `http://127.0.0.1:${PORT}`;

async function main() {
  const { server } = spawnServer('echo', PORT);
  await waitForPort(PORT);
  try {
    // Non-scoped `/v1/ag-ui` run with plain-string message content.
    const res = await fetch(`${BASE}/v1/ag-ui`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        threadId: 'agextra',
        messages: [{ id: 'm1', role: 'user', content: 'AG-PLAIN' }],
      }),
    });
    assert.equal(res.status, 200, 'non-scoped run accepted');
    const text = await res.text();
    assert.ok(text.includes('Echo: AG-PLAIN'), 'the non-scoped run streamed the reply');
    assert.ok(text.includes('RUN_STARTED') || text.includes('RunStarted') || text.includes('TEXT_MESSAGE'), 'AG-UI lifecycle events present');
    pass('non-scoped /v1/ag-ui run streamed a reply (run_agent, plain-string content)');

    // Malformed body → an AG-UI error event, not a crash.
    const bad = await fetch(`${BASE}/v1/ag-ui`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: '{ not valid',
    });
    const badText = await bad.text();
    assert.ok(badText.toUpperCase().includes('ERROR'), 'a malformed body yields an error event');
    pass('malformed body failed closed with an AG-UI error event (AgUiJson decode path)');

    console.log('E2E PASS: ag-ui extra endpoints and error paths.');
  } finally {
    await stopServer(server);
  }
}

main().catch((err) => {
  console.error('E2E FAIL:', err);
  process.exitCode = 1;
});
