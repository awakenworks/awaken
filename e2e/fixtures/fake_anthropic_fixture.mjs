// A minimal fake Anthropic-compatible upstream: POST …/messages answers the
// Messages shape with a deterministic `FAKE:<last user text>` reply when the
// caller presents the expected key, and the Anthropic 401 error envelope
// otherwise. Lets e2e drive the REAL provider path (provider-genai over the
// wire) and the credential-validation probe without a live key.

import http from 'node:http';

// `opts`: `{ failuresBeforeSuccess, alwaysFail, faultStatus }` inject retryable
// upstream failures so e2e can drive the runtime's retry + circuit-breaker + error
// paths without a live key. Default (no opts) is the original always-succeed fake.
export function startFakeAnthropic(apiKey, opts = {}) {
  const { failuresBeforeSuccess = 0, alwaysFail = false, faultStatus = 503, delayMs = 0 } = opts;
  const state = { requests: [], unauthorized: 0, attempts: 0 };
  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', async () => {
      // Hold the response so a turn stays in flight (lets e2e drive the live-inbox
      // while a turn is running). Only the authenticated success path is delayed.
      if (delayMs > 0) await new Promise((r) => setTimeout(r, delayMs));
      const presented = req.headers['x-api-key'] ?? (req.headers.authorization ?? '').replace(/^Bearer /, '');
      if (req.method !== 'POST' || !req.url.endsWith('/messages')) {
        res.writeHead(404, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ type: 'error', error: { type: 'not_found_error', message: `no ${req.method} ${req.url}` } }));
        return;
      }
      if (presented !== apiKey) {
        state.unauthorized += 1;
        res.writeHead(401, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ type: 'error', error: { type: 'authentication_error', message: 'invalid x-api-key' } }));
        return;
      }
      // Fault injection: fail the first N authenticated attempts (retry recovers)
      // or every attempt (retries exhausted → the run loop surfaces an error).
      state.attempts += 1;
      if (alwaysFail || state.attempts <= failuresBeforeSuccess) {
        res.writeHead(faultStatus, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ type: 'error', error: { type: 'overloaded_error', message: 'fake upstream is overloaded' } }));
        return;
      }
      const parsed = JSON.parse(body || '{}');
      state.requests.push({ url: req.url, model: parsed.model, stream: !!parsed.stream });
      const lastUser = [...(parsed.messages ?? [])].reverse().find((m) => m.role === 'user');
      const text =
        typeof lastUser?.content === 'string'
          ? lastUser.content
          : (lastUser?.content ?? []).filter((b) => b.type === 'text').map((b) => b.text).join('');
      const reply = `FAKE:${text}`;
      const id = `msg_${state.requests.length}`;
      const model = parsed.model ?? 'fake-model';
      // `use-tool:<name>` drives one tool round-trip: first call answers with a
      // tool_use block, the follow-up (carrying the tool_result) answers text.
      const hasToolResult = (parsed.messages ?? []).some(
        (m) => Array.isArray(m.content) && m.content.some((b) => b.type === 'tool_result'),
      );
      const toolMatch = !hasToolResult && /^use-tool:(\S+)/.exec(text);
      if (parsed.stream && toolMatch) {
        const tool = toolMatch[1];
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        const ev = (event, data) => res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
        ev('message_start', {
          type: 'message_start',
          message: {
            id, type: 'message', role: 'assistant', model,
            content: [], stop_reason: null, stop_sequence: null,
            usage: { input_tokens: 1, output_tokens: 0 },
          },
        });
        ev('content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'tool_use', id: `toolu_${id}`, name: tool, input: {} } });
        ev('content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'input_json_delta', partial_json: '{"pattern":"*.md"}' } });
        ev('content_block_stop', { type: 'content_block_stop', index: 0 });
        ev('message_delta', { type: 'message_delta', delta: { stop_reason: 'tool_use', stop_sequence: null }, usage: { output_tokens: 1 } });
        ev('message_stop', { type: 'message_stop' });
        res.end();
        return;
      }
      if (parsed.stream) {
        // The Anthropic streaming wire: the fixed event ladder around one text block.
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        const ev = (event, data) => res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
        ev('message_start', {
          type: 'message_start',
          message: {
            id, type: 'message', role: 'assistant', model,
            content: [], stop_reason: null, stop_sequence: null,
            usage: { input_tokens: 1, output_tokens: 0 },
          },
        });
        ev('content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } });
        ev('content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: reply } });
        ev('content_block_stop', { type: 'content_block_stop', index: 0 });
        ev('message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 1 } });
        ev('message_stop', { type: 'message_stop' });
        res.end();
        return;
      }
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(
        JSON.stringify({
          id, type: 'message', role: 'assistant', model,
          content: [{ type: 'text', text: reply }],
          stop_reason: 'end_turn',
          stop_sequence: null,
          usage: { input_tokens: 1, output_tokens: 1 },
        }),
      );
    });
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      resolve({
        url: `http://127.0.0.1:${port}`,
        requests: state.requests,
        get unauthorized() {
          return state.unauthorized;
        },
        get attempts() {
          return state.attempts;
        },
        close: () => server.close(),
      });
    });
  });
}
