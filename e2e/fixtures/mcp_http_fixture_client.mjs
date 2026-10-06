// One HTTP MCP client for ACP process fixtures; no credential custody.
export async function mcpRequest(url, method, params, state) {
  const headers = {
    accept: 'application/json, text/event-stream',
    'content-type': 'application/json',
    ...state.headers,
    ...(state.sessionId ? { 'mcp-session-id': state.sessionId } : {}),
  };
  const response = await fetch(url, {
    method: 'POST',
    headers,
    body: JSON.stringify({ jsonrpc: '2.0', id: state.nextId++, method, params }),
  });
  if (!response.ok) throw new Error(`MCP ${method} returned ${response.status}: ${await response.text()}`);
  state.sessionId ??= response.headers.get('mcp-session-id');
  const body = await response.text();
  const payload = body.split('\n')
    .map((line) => line.trim())
    .filter((line) => line.startsWith('data:'))
    .map((line) => line.slice(5).trim())
    .find((line) => line && line !== '[DONE]') ?? body;
  const decoded = JSON.parse(payload);
  if (decoded.error) throw new Error(`MCP ${method} failed: ${JSON.stringify(decoded.error)}`);
  return decoded.result;
}

export async function mcpNotify(url, method, state) {
  const response = await fetch(url, {
    method: 'POST',
    headers: {
      accept: 'application/json, text/event-stream',
      'content-type': 'application/json',
    ...state.headers,
      ...(state.sessionId ? { 'mcp-session-id': state.sessionId } : {}),
    },
    body: JSON.stringify({ jsonrpc: '2.0', method }),
  });
  if (!response.ok) throw new Error(`MCP ${method} returned ${response.status}: ${await response.text()}`);
}
