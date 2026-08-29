import assert from 'node:assert/strict';
import http from 'node:http';
import Anthropic from '@anthropic-ai/sdk';

// Cause/effect graph: C1 each SDK Tunnel/Certificate method, C2 research beta
// header present, C3 official path and body. Effects: E1 request reaches the one
// wire owner, E2 response decodes to the SDK union with standard response
// context, E3 token responses are never cached. Decision table:
// T1=C1+C2+C3+request/workspace headers -> E1+E2; T2=!C2 -> server rejection is
// owned by Rust middleware; T3=reveal/rotate -> E2+E3. Cloud lifecycle behavior
// is intentionally a separate Full-target suite over the same public artifact.

const beta = 'mcp-tunnels-2026-06-22';
const observed = [];
const tunnel = {
  id: 'tnl_contract', type: 'tunnel', archived_at: null,
  created_at: '2026-08-12T00:00:00Z', display_name: 'contract',
  domain: 'tnl-contract.example.test',
};
const certificate = {
  id: 'tcrt_contract', type: 'tunnel_certificate', archived_at: null,
  created_at: '2026-08-12T00:00:00Z', expires_at: null,
  fingerprint: 'a'.repeat(64), tunnel_id: tunnel.id,
};

const server = http.createServer(async (request, response) => {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  const body = chunks.length ? JSON.parse(Buffer.concat(chunks).toString()) : null;
  observed.push({ method: request.method, url: request.url, beta: request.headers['anthropic-beta'], body });
  assert.match(request.headers['anthropic-beta'] ?? '', new RegExp(beta), 'T1 beta');
  response.setHeader('content-type', 'application/json');
  response.setHeader('request-id', `req_tunnel_contract_${observed.length}`);
  response.setHeader('anthropic-workspace-id', 'workspace_tunnel_contract');
  if (request.url.includes('/certificates')) {
    response.end(JSON.stringify(request.method === 'GET' && !request.url.includes(certificate.id)
      ? { data: [certificate], next_page: null }
      : certificate));
  } else if (request.url.includes('reveal_token') || request.url.includes('rotate_token')) {
    response.setHeader('cache-control', 'no-store');
    response.end(JSON.stringify({ id: 'ttok_contract', type: 'tunnel_token', tunnel_token: 'secret' })); // awaken-allow: secret
  } else if (request.method === 'GET' && request.url.startsWith('/v1/tunnels?')) {
    response.end(JSON.stringify({ data: [tunnel], next_page: null }));
  } else {
    response.end(JSON.stringify(tunnel));
  }
});

await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
try {
  const { port } = server.address();
  const client = new Anthropic({ apiKey: 'contract-key', baseURL: `http://127.0.0.1:${port}` }); // awaken-allow: secret -- inert loopback fixture
  const betas = [beta];
  const created = await client.beta.tunnels.create({ display_name: 'contract', betas }).withResponse();
  assert.equal(created.data.id, tunnel.id, 'T1 create');
  assert.match(created.response.headers.get('request-id') ?? '', /^req_tunnel_contract_\d+$/u);
  assert.equal(
    created.response.headers.get('anthropic-workspace-id'),
    'workspace_tunnel_contract',
  );
  await client.beta.tunnels.retrieve(tunnel.id, { betas });
  for await (const row of client.beta.tunnels.list({ betas })) assert.equal(row.id, tunnel.id, 'T1 list');
  await client.beta.tunnels.archive(tunnel.id, { betas });
  const revealed = await client.beta.tunnels.revealToken(tunnel.id, { betas });
  assert.equal(revealed.tunnel_token, 'secret', 'T3 reveal');
  await client.beta.tunnels.rotateToken(tunnel.id, { reason: 'contract', betas });
  await client.beta.tunnels.certificates.create(tunnel.id, { ca_certificate_pem: 'certificate', betas });
  await client.beta.tunnels.certificates.retrieve(certificate.id, { tunnel_id: tunnel.id, betas });
  for await (const row of client.beta.tunnels.certificates.list(tunnel.id, { betas })) {
    assert.equal(row.id, certificate.id, 'T1 certificate list');
  }
  await client.beta.tunnels.certificates.archive(certificate.id, { tunnel_id: tunnel.id, betas });
  assert.equal(observed.length, 10, 'all SDK methods reached one wire owner');
  console.log('MANAGED TUNNELS CONTRACT E2E PASS');
} finally {
  await new Promise((resolve) => server.close(resolve));
}
