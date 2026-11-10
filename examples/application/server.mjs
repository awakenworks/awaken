// Loopback-only, one-Session example backend. Awaken owns tokens and history.
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { Readable } from 'node:stream';
import { fileURLToPath } from 'node:url';
import { resolve, sep } from 'node:path';

const upstream = new URL(process.env.AWAKEN_BASE_URL ?? 'http://127.0.0.1:8080');
const sessionId = process.env.AWAKEN_SESSION_ID;
const threadId = 'example-chat';
const port = Number(process.env.PORT ?? 4317);
if (!sessionId) throw new Error('Set AWAKEN_SESSION_ID to the Session printed by the Managed example');
const directory = resolve(fileURLToPath(new URL('./dist/', import.meta.url)));

const server = createServer(async (request, response) => {
  const controller = new AbortController();
  response.on('close', () => controller.abort());
  const pathname = new URL(request.url, 'http://example.invalid').pathname;
  try {
    const host = new URL(`http://${request.headers.host}`);
    if (!['127.0.0.1', 'localhost'].includes(host.hostname) || Number(host.port) !== server.address().port) {
      response.writeHead(403).end('Unexpected host'); return;
    }
    // This example has one operator and one preselected Session. A real product
    // authorizes its user before minting the same existing application token.
    if (request.method === 'POST' && pathname === '/api/connection') {
      if (request.headers.origin && request.headers.origin !== `http://${request.headers.host}`) {
        response.writeHead(403).end('Foreign origin'); return;
      }
      const result = await fetch(new URL('/v1/application-access-tokens', upstream), {
        method: 'POST', signal: controller.signal,
        headers: {
          Authorization: `Bearer ${process.env.AWAKEN_API_KEY ?? 'local'}`,
          'content-type': 'application/json',
          ...(process.env.AWAKEN_WORKSPACE_ID ? { 'anthropic-workspace-id': process.env.AWAKEN_WORKSPACE_ID } : {}),
        },
        body: JSON.stringify({
          protocols: ['ai-sdk'], operations: ['thread.run', 'thread.messages.read'],
          thread_bindings: [{ external_thread_id: threadId, managed_session_id: sessionId }],
          expires_in_seconds: 300,
        }),
      });
      if (!result.ok) { response.writeHead(result.status).end('Token issuance failed; inspect the backend configuration'); return; }
      const token = await result.json();
      response.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' });
      response.end(JSON.stringify({ access_token: token.access_token, expires_at: token.expires_at, thread_id: threadId }));
      return;
    }
    // A narrow same-origin proxy carries only the browser's application token,
    // never the backend service key. It preserves streaming and Stop's receipt.
    const prefix = `/awaken/v1/ai-sdk/threads/${threadId}/`;
    if (pathname.startsWith(prefix)) {
      const resource = pathname.slice(prefix.length);
      if (!((resource === 'messages' && request.method === 'GET')
        || (resource === 'runs' && ['POST', 'DELETE'].includes(request.method)))) {
        response.writeHead(404).end(); return;
      }
      const headers = { accept: request.headers.accept ?? 'application/json' };
      if (request.headers.authorization) headers.authorization = request.headers.authorization;
      if (request.headers['content-type']) headers['content-type'] = request.headers['content-type'];
      const result = await fetch(new URL(pathname.slice('/awaken'.length), upstream), {
        method: request.method, headers, signal: controller.signal,
        ...(request.method === 'POST' ? { body: Readable.toWeb(request), duplex: 'half' } : {}),
      });
      const resultHeaders = { 'cache-control': 'no-store' };
      for (const name of ['content-type', 'x-vercel-ai-ui-message-stream']) {
        if (result.headers.has(name)) resultHeaders[name] = result.headers.get(name);
      }
      response.writeHead(result.status, resultHeaders);
      if (result.body) {
        const stream = Readable.fromWeb(result.body);
        // A browser detach aborts fetch; its body can emit asynchronously after
        // this handler returns. Close that response without crashing the backend.
        stream.once('error', () => response.destroy());
        stream.pipe(response);
      } else response.end();
      return;
    }
    if (request.method !== 'GET') { response.writeHead(404).end(); return; }
    const path = resolve(directory, `.${decodeURIComponent(pathname === '/' ? '/index.html' : pathname)}`);
    if (!path.startsWith(directory + sep)) { response.writeHead(404).end(); return; }
    const body = await readFile(path);
    const type = path.endsWith('.html') ? 'text/html' : path.endsWith('.js') ? 'application/javascript' : 'text/css';
    response.writeHead(200, { 'content-type': type, 'cache-control': 'no-store' }); response.end(body);
  } catch (error) {
    if (controller.signal.aborted) return;
    if (!response.headersSent) response.writeHead(error.code === 'ENOENT' ? 404 : 502);
    response.end('Example request failed; check the service and published Session');
  }
});
server.listen(port, '127.0.0.1', () => console.log(`Application example: http://127.0.0.1:${server.address().port}`));
for (const signal of ['SIGTERM', 'SIGINT']) process.once(signal, () => server.close());
