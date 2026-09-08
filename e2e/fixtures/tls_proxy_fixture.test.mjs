import assert from 'node:assert/strict';
import test from 'node:test';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createServer } from 'node:http';
import { request } from 'node:https';
import { createTlsIdentityFixture } from './tls_identity_fixture.mjs';
import { startTlsProxy, stopTlsProxy } from './tls_proxy_fixture.mjs';

test('shared TLS edge preserves bytes and fails closed at the transport boundaries', async () => {
  // Cause/effect table: C1 exact trusted CA; C2 upstream available; C3 bound
  // listener; C4 certificate files readable. T1 C1+C2+C3+C4 => exact method,
  // path, request header/body and response status/header/body preserved.
  // T2 !C1 => TLS rejection, zero upstream calls; T3 !C2 => explicit 502;
  // T4 port already held or !C4 => rejected startup, no silent fallback.
  // This validates fixture mechanics, NOT product signing or authorization.
  const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-tls-edge-'));
  let proxy;
  let upstream;
  let calls = 0;
  try {
    const identity = createTlsIdentityFixture(temp, {
      caCommonName: 'Fixture transport CA', serverCommonName: '127.0.0.1',
      subjectAltNames: ['IP:127.0.0.1'],
    });
    const payload = '独立 Worker\u0000bytes';
    upstream = createServer(async (incoming, outgoing) => {
      calls++;
      assert.equal(incoming.method, 'POST');
      assert.equal(incoming.url, '/exact?q=%2F');
      assert.equal(incoming.headers['x-fixture'], 'exact-header');
      const chunks = [];
      for await (const chunk of incoming) chunks.push(chunk);
      assert.equal(Buffer.concat(chunks).toString(), payload);
      outgoing.writeHead(201, { 'x-fixture-reply': 'preserved' });
      outgoing.end(payload);
    });
    await new Promise((resolve) => upstream.listen(0, '127.0.0.1', resolve));
    proxy = await startTlsProxy(0, upstream.address().port, identity.serverCertificate, identity.serverKey);
    const call = (trusted) => new Promise((resolve, reject) => {
      const outgoing = request({
        hostname: '127.0.0.1', port: proxy.address().port, path: '/exact?q=%2F',
        method: 'POST', headers: { 'x-fixture': 'exact-header' },
        ...(trusted ? { ca: fs.readFileSync(identity.caCertificate) } : {}),
      }, async (response) => {
        const chunks = [];
        for await (const chunk of response) chunks.push(chunk);
        resolve({ status: response.statusCode, header: response.headers['x-fixture-reply'],
          body: Buffer.concat(chunks).toString() });
      });
      outgoing.on('error', reject);
      outgoing.end(payload);
    });
    assert.deepEqual(await call(true), { status: 201, header: 'preserved', body: payload }, 'T1');
    await assert.rejects(call(false), /certificate/u, 'T2');
    assert.equal(calls, 1, 'T2 never reached the upstream');
    await assert.rejects(startTlsProxy(proxy.address().port, upstream.address().port,
      identity.serverCertificate, identity.serverKey), { code: 'EADDRINUSE' }, 'T4 occupied port');
    await assert.rejects(startTlsProxy(0, upstream.address().port,
      path.join(temp, 'missing.pem'), identity.serverKey), { code: 'ENOENT' }, 'T4 missing identity');
    await new Promise((resolve) => upstream.close(resolve));
    assert.equal((await call(true)).status, 502, 'T3 reports unavailable upstream');
    assert.equal(calls, 1, 'T3 no fallback upstream');
  } finally {
    if (proxy) await stopTlsProxy(proxy);
    if (upstream?.listening) await new Promise((resolve) => upstream.close(resolve));
    fs.rmSync(temp, { recursive: true, force: true });
  }
});
