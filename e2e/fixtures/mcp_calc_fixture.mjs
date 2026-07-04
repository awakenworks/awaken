// Mock `calc` MCP server fixture for the MCP e2e scenarios. Mirrors the
// in-process Rust mock in crates/agents/awaken-server-local/tests/mcp_sessions.rs
// exactly: JSON-RPC 2.0 over POST on 127.0.0.1, `initialize` → a standard
// result, `notifications/initialized` (no id) → 202 with no body,
// `tools/list` → one `add` tool, `tools/call`(add) → the sum as a text block.
// EVERY request must carry `Authorization: Bearer <token>`, else 401 with a
// WWW-Authenticate challenge. The fixture records each request's method and
// auth header so tests can assert the vault-materialized bearer arrived.

import http from 'node:http';

/// Start the fixture; resolves to { url, calls, unauthorized, close() }.
/// `calls` is [{ method, authorization }] for every authorized JSON-RPC POST;
/// `unauthorized` counts requests rejected for a missing/wrong bearer.
export function startCalcFixture(token) {
  const calls = [];
  const state = { unauthorized: 0 };

  const server = http.createServer((req, res) => {
    const expected = `Bearer ${token}`;
    if (req.headers.authorization !== expected) {
      state.unauthorized += 1;
      res.writeHead(401, { 'WWW-Authenticate': 'Bearer resource_metadata="none"' });
      res.end();
      return;
    }
    // The awaken-ext-mcp transport also opens a background GET SSE listener; a
    // 405 tells it this server has no standalone stream (it stops cleanly).
    if (req.method !== 'POST') {
      res.writeHead(405);
      res.end();
      return;
    }
    let raw = '';
    req.on('data', (chunk) => { raw += chunk; });
    req.on('end', () => {
      let body;
      try {
        body = JSON.parse(raw);
      } catch {
        res.writeHead(400);
        res.end();
        return;
      }
      calls.push({ method: body.method, authorization: req.headers.authorization });

      let result;
      switch (body.method) {
        case 'initialize':
          result = {
            protocolVersion: '2025-06-18',
            capabilities: {},
            serverInfo: { name: 'calc', version: '0.0.1' },
          };
          break;
        case 'tools/list':
          result = {
            tools: [{
              name: 'add',
              description: 'Add two integers.',
              inputSchema: {
                type: 'object',
                properties: { a: { type: 'integer' }, b: { type: 'integer' } },
                required: ['a', 'b'],
              },
            }],
          };
          break;
        case 'tools/call': {
          const args = body.params?.arguments ?? {};
          const sum = Number(args.a ?? 0) + Number(args.b ?? 0);
          result = { content: [{ type: 'text', text: String(sum) }], isError: false };
          break;
        }
        default:
          // Notifications (e.g. `notifications/initialized`) carry no id and
          // get no JSON-RPC response — a bare 202 acknowledgement suffices.
          res.writeHead(202);
          res.end();
          return;
      }
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ jsonrpc: '2.0', id: body.id, result }));
    });
  });

  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      resolve({
        url: `http://127.0.0.1:${port}/`,
        calls,
        get unauthorized() { return state.unauthorized; },
        close: () => new Promise((done) => server.close(done)),
      });
    });
  });
}
