import fs from 'node:fs';
import { request as httpRequest } from 'node:http';
import { createServer as createHttpsServer } from 'node:https';

// Shared transport mechanics, moved from the production materialization Worker
// E2E. This terminates explicit fixture TLS only: product authentication,
// certificate validation, route admission and claim fencing stay unchanged.
export async function startTlsProxy(port, targetPort, certificate, key) {
  const server = createHttpsServer(
    { cert: fs.readFileSync(certificate), key: fs.readFileSync(key) },
    (incoming, outgoing) => {
      const upstream = httpRequest({
        hostname: '127.0.0.1', port: targetPort, method: incoming.method,
        path: incoming.url, headers: incoming.headers,
      }, (response) => {
        outgoing.writeHead(response.statusCode ?? 502, response.headers);
        response.pipe(outgoing);
      });
      upstream.on('error', (error) => {
        if (!outgoing.headersSent) outgoing.writeHead(502);
        outgoing.end(`TLS proxy upstream failed: ${error.message}`);
      });
      incoming.pipe(upstream);
    },
  );
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, '127.0.0.1', () => {
      server.off('error', reject);
      resolve();
    });
  });
  return server;
}

export async function stopTlsProxy(server) {
  await new Promise((resolve, reject) => {
    server.close((error) => (error ? reject(error) : resolve()));
  });
}
