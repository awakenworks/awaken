// One HTTP MCP client for ACP process fixtures; no credential custody.
async function postMcp(url, method, fields, state) {
  const headers = {
    accept: 'application/json, text/event-stream',
    'content-type': 'application/json',
    ...state.headers,
    ...(state.sessionId ? { 'mcp-session-id': state.sessionId } : {}),
  };
  const response = await fetch(url, {
    method: 'POST',
    headers,
    body: JSON.stringify({ jsonrpc: '2.0', method, ...fields }),
  });
  if (!response.ok) throw new Error(`MCP ${method} returned ${response.status}: ${await response.text()}`);
  return response;
}

export async function mcpRequest(url, method, params, state) {
  const response = await postMcp(url, method, { id: state.nextId++, params }, state);
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
  await postMcp(url, method, {}, state);
}
