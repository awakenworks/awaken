// Mock `calc` MCP server fixture for the MCP e2e scenarios. Mirrors the
// in-process Rust mocks in crates/agents/awaken-server-local/tests/mcp_sessions.rs
// exactly: JSON-RPC 2.0 over POST on 127.0.0.1, `initialize` → a standard
// result, `notifications/initialized` (no id) → 202 with no body,
// `tools/list` → one `add` tool, `tools/call`(add) → the sum as a text block.
// EVERY request must carry an accepted `Authorization: Bearer <token>`, else
// 401 with a WWW-Authenticate challenge. The fixture records each request's
// method and auth header so tests can assert the vault-materialized bearer
// arrived.
//
// OAuth mode (mirrors `mock_oauth_calc_mcp`): `POST /token` is the refresh
// token endpoint. It records every raw form-encoded grant; when the body is
// `grant_type=refresh_token&refresh_token=<the configured refreshToken>` it
// issues `issueToken` (which becomes an accepted bearer, optionally rotating
// the refresh token in the response), otherwise it refuses with
// 400 `invalid_grant`. With `expiredInitial: true` the constructor token is
// NEVER accepted — every bearer 401s until a grant is made, after which only
// the issued token is accepted. With `clientAuth` the grant must also
// authenticate the client (401 `invalid_client` otherwise): method 'basic'
// requires the RFC 6749 §2.3.1 `Authorization: Basic
// base64(urlencode(client_id):urlencode(client_secret))` header AND rejects a
// form-body client_id; method 'post' requires `client_secret=<...>` in the
// form body.

import http from 'node:http';

/// Start the fixture; resolves to
///   { url, tokenUrl, calls, grants, tokenRequests, unauthorized, close() }.
/// `calls` is [{ method, authorization }] for every AUTHORIZED JSON-RPC POST;
/// `grants` is [{ body, contentType, authorization }] for every POST /token
/// grant attempt; `tokenRequests` maps a presented bearer token → its JSON-RPC
/// POST count (authorized or not, `/token` excluded); `unauthorized` counts
/// requests rejected for a missing/wrong bearer.
///
/// Options (all optional — the default is the original static-bearer fixture):
///   expiredInitial  when true, `token` is never accepted; only tokens issued
///                   by a /token grant are (the "expired initial token" mode).
///   refreshToken    the fixed refresh token /token honors; null (default)
///                   refuses every grant with 400 invalid_grant.
///   issueToken      the access token a successful grant issues ('new-token').
///   rotateRefreshTo when set, a successful grant also returns this rotated
///                   `refresh_token` in its response body.
///   clientAuth      { method: 'basic'|'post', clientSecret, clientId? } —
///                   the client authentication a grant must present, verified
///                   before the refresh token: 'basic' = the §2.3.1 Basic
///                   header (decoded + form-urldecoded halves must match, and
///                   the form body must NOT carry a client_id); 'post' =
///                   `client_secret` in the form body. Wrong/missing → 401
///                   `invalid_client`.
export function startCalcFixture(token, options = {}) {
  const {
    expiredInitial = false,
    refreshToken = null,
    issueToken = 'new-token', // awaken-allow: secret
    rotateRefreshTo = null,
    clientAuth = null,
  } = options;

  // Undo application/x-www-form-urlencoded encoding ('+' is a space).
  const formUrldecode = (s) => decodeURIComponent(s.replace(/\+/g, '%20'));

  /// Whether a /token grant presented the configured client authentication.
  function clientAuthenticated(rawBody, authorization) {
    if (clientAuth === null) return true;
    const form = new URLSearchParams(rawBody);
    if (clientAuth.method === 'basic') {
      if (!authorization?.startsWith('Basic ') || form.has('client_id')) return false;
      const pair = Buffer.from(authorization.slice('Basic '.length), 'base64').toString('utf8');
      const colon = pair.indexOf(':');
      if (colon < 0) return false;
      const id = formUrldecode(pair.slice(0, colon));
      const secret = formUrldecode(pair.slice(colon + 1));
      return secret === clientAuth.clientSecret &&
        (clientAuth.clientId == null || id === clientAuth.clientId);
    }
    return form.get('client_secret') === clientAuth.clientSecret;
  }

  const calls = [];
  const grants = [];
  const tokenRequests = Object.create(null);
  const validTokens = new Set(expiredInitial ? [] : [token]);
  const state = { unauthorized: 0 };

  const server = http.createServer((req, res) => {
    // --- the OAuth token endpoint: no bearer required (public client) ---
    if (req.url === '/token') {
      if (req.method !== 'POST') {
        res.writeHead(405);
        res.end();
        return;
      }
      let raw = '';
      req.on('data', (chunk) => { raw += chunk; });
      req.on('end', () => {
        const authorization = req.headers.authorization ?? null;
        grants.push({ body: raw, contentType: req.headers['content-type'] ?? '', authorization });
        // Client authentication first (RFC 6749 §2.3): a client that cannot
        // authenticate never gets a token, whatever its refresh token says.
        if (!clientAuthenticated(raw, authorization)) {
          res.writeHead(401, { 'content-type': 'application/json' });
          res.end(JSON.stringify({ error: 'invalid_client' }));
          return;
        }
        const form = new URLSearchParams(raw);
        const honored =
          refreshToken !== null &&
          form.get('grant_type') === 'refresh_token' &&
          form.get('refresh_token') === refreshToken;
        if (!honored) {
          res.writeHead(400, { 'content-type': 'application/json' });
          res.end(JSON.stringify({ error: 'invalid_grant' }));
          return;
        }
        validTokens.add(issueToken);
        const issued = { access_token: issueToken, token_type: 'Bearer' };
        if (rotateRefreshTo !== null) issued.refresh_token = rotateRefreshTo;
        res.writeHead(200, { 'content-type': 'application/json' });
        res.end(JSON.stringify(issued));
      });
      return;
    }

    // --- the JSON-RPC endpoint: every request needs an accepted bearer ---
    const bearer = (req.headers.authorization ?? '').replace(/^Bearer /, '');
    // Per-token counts cover JSON-RPC POSTs only (mirrors the Rust mock's
    // POST-routed `requests`); the transport's background GET SSE probe is
    // not a JSON-RPC request.
    if (req.method === 'POST') tokenRequests[bearer] = (tokenRequests[bearer] ?? 0) + 1;
    if (!validTokens.has(bearer) || !req.headers.authorization?.startsWith('Bearer ')) {
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
        tokenUrl: `http://127.0.0.1:${port}/token`,
        calls,
        grants,
        tokenRequests,
        get unauthorized() { return state.unauthorized; },
        close: () => new Promise((done) => server.close(done)),
      });
    });
  });
}
