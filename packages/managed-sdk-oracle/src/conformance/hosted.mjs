// Canonical Managed behavior qualification owned by Open Awaken. Target-specific
// composition supplies only endpoint, credentials, fixtures, and failure controls.

import assert from 'node:assert/strict';
import http from 'node:http';
import https from 'node:https';

import { loadQualifiedClients, qualifiedClient } from './clients.mjs';
import { exerciseUserProfileChangePoint } from './user-profile-change-point.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const FILE_BETAS = [...BETAS, 'files-api-2025-04-14'];
const TUNNEL_BETAS = ['mcp-tunnels-2026-06-22'];
const LEGACY_TUNNEL_BETA = 'mcp-tunnels-2026-05-19';
// Test-only public CA. The private key is not retained; the certificate has
// critical CA:TRUE, keyCertSign, SKI, P-256 and SHA-256, matching the public
// Tunnel admission policy through 2036-08-15.
const TUNNEL_CA_PEM = `-----BEGIN CERTIFICATE-----
MIIBrTCCAVOgAwIBAgIUWe1FK0PD2YlVeK9WRInAl2YA8SAwCgYIKoZIzj0EAwIw
JDEiMCAGA1UEAwwZYXdha2VuLW1hbmFnZWQtc2RrLWUyZS1jYTAeFw0yNjA4MTgx
MzM3MjJaFw0zNjA4MTUxMzM3MjJaMCQxIjAgBgNVBAMMGWF3YWtlbi1tYW5hZ2Vk
LXNkay1lMmUtY2EwWTATBgcqhkjOPQIBBggqhkjOPQMBBwNCAASUrEyRAYypoZUl
qcmtHps/SxkGqoT+4IQKD/k9a+uBTc4cNUwTta0Y8OZpQrJ97Vkx5hEBY1aqGtjC
Yq+NbTHIo2MwYTAfBgNVHSMEGDAWgBTx0QpnYesoIbUJ4Up0uvxbunKjETAPBgNV
HRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwICBDAdBgNVHQ4EFgQU8dEKZ2HrKCG1
CeFKdLr8W7pyoxEwCgYIKoZIzj0EAwIDSAAwRQIgS4B6fQj5UHT+4K9gknAevKBq
8k0PPK18p/elo3zGUcwCIQDZxbSfMgPw97DvxbmjL+NLlXlclN+jg/3u+XSbsuno
9Q==
-----END CERTIFICATE-----`;
const clients = await loadQualifiedClients();
const baseURL = process.env.AWAKEN_MANAGED_BASE_URL;
const apiKey = process.env.AWAKEN_MANAGED_API_KEY;
const tunnelAccessToken = process.env.AWAKEN_MANAGED_TUNNEL_ACCESS_TOKEN;
const agent = process.env.AWAKEN_MANAGED_AGENT_ID;
const environmentId = process.env.AWAKEN_MANAGED_ENVIRONMENT_ID;
const workspaceId = process.env.AWAKEN_MANAGED_WORKSPACE_ID;
const userProfileId = process.env.AWAKEN_MANAGED_USER_PROFILE_ID;
const userProfileAccessType = process.env.AWAKEN_MANAGED_USER_PROFILE_ACCESS_TYPE;

for (const [name, value] of Object.entries({
  baseURL, apiKey, tunnelAccessToken, agent, environmentId, workspaceId,
  userProfileId, userProfileAccessType,
})) {
  assert.ok(value, `${name} is required`);
}
function ingressRequest(path, headerPairs) {
  const url = new URL(path, baseURL);
  const transport = url.protocol === 'https:' ? https : http;
  return new Promise((resolve, reject) => {
    const request = transport.request(url, {
      method: 'GET',
      // The array form deliberately preserves repeated field lines until the
      // public ingress. NGINX/Envoy may legally comma-join them, but must not
      // drop, reorder into another capability, or privately select a schema.
      headers: headerPairs.flatMap(([name, value]) => [name, value]),
    }, (response) => {
      response.resume();
      response.on('end', () => resolve(response.statusCode));
    });
    request.on('error', reject);
    request.end();
  });
}

async function exerciseIngressHeaderFidelity() {
  const identity = [
    ['x-api-key', apiKey],
    ['anthropic-version', '2023-06-01'],
  ];
  const direct = '/v1/memory_stores?beta=true';
  const scoped = `/v1/workspaces/${encodeURIComponent(workspaceId)}/memory_stores?beta=true`;
  for (const path of [direct, scoped]) {
    assert.equal(await ingressRequest(path, identity), 400, `${path}: missing beta`);
    assert.equal(await ingressRequest(path, [...identity, ['anthropic-beta', 'agent-memory-2026-07-22']]), 200);
    assert.equal(await ingressRequest(path, [...identity, ['anthropic-beta', BETAS[0]]]), 200);
  }
  assert.equal(await ingressRequest(direct, [
    ...identity,
    ['anthropic-beta', 'agent-memory-2026-07-22'],
    ['anthropic-beta', 'agent-memory-2026-07-22'],
  ]), 200, 'public ingress preserves repeated identical beta fields');
  for (const values of [
    [BETAS[0], 'agent-memory-2026-07-22'],
    ['agent-memory-2026-07-22', BETAS[0]],
  ]) {
    assert.equal(await ingressRequest(direct, [
      ...identity,
      ...values.map((value) => ['anthropic-beta', value]),
    ]), 400, 'public ingress cannot hide ambiguous capability order');
  }
  assert.equal(await ingressRequest(direct, [
    ...identity,
    ['anthropic-beta', '  agent-memory-2026-07-22, agent-memory-2026-07-22  '],
  ]), 200, 'comma joining and optional whitespace preserve the Memory selector');
  assert.equal(await ingressRequest(direct, [
    ...identity,
    ['anthropic-beta', 'future-memory-beta'],
  ]), 400, 'unknown-only beta remains rejected after public ingress');
}

async function eventsUntilIdle(client, sessionId) {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const events = [];
    for await (const event of client.beta.sessions.events.list(sessionId, { betas: BETAS })) {
      events.push(event);
    }
    if (events.some((event) => event.type === 'session.status_idle')) return events;
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  throw new Error(`Session ${sessionId} did not become idle`);
}

async function drain(page) {
  const values = [];
  for await (const value of page) values.push(value);
  return values;
}

async function exercise(version, Client, toFile) {
  const client = new Client({ apiKey, baseURL });
  const file = await client.beta.files.upload({
    file: await toFile(Buffer.from(`cloud-sdk-matrix-file-${version}`), `matrix-${version}.txt`),
    betas: FILE_BETAS,
  });
  let session;
  try {
    session = await client.beta.sessions.create({
      agent,
      environment_id: environmentId,
      resources: [{
        type: 'file',
        file_id: file.id,
        mount_path: `/workspace/matrix-${version}.txt`,
      }],
      betas: FILE_BETAS,
    });
    assert.equal(session.type, 'session', `${version}: create`);
    const retrieved = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(retrieved.id, session.id, `${version}: retrieve`);
    const updated = await client.beta.sessions.update(session.id, {
      title: `Hosted SDK matrix ${version}`,
      metadata: { compatibility_anchor: version },
      betas: BETAS,
    });
    assert.equal(updated.title, `Hosted SDK matrix ${version}`, `${version}: update`);
    assert.equal(updated.metadata?.compatibility_anchor, version, `${version}: metadata`);

    const listed = await drain(client.beta.sessions.list({ limit: 100, betas: BETAS }));
    assert.ok(listed.some((candidate) => candidate.id === session.id), `${version}: list`);
    const resources = await drain(client.beta.sessions.resources.list(session.id, {
      betas: FILE_BETAS,
    }));
    assert.ok(
      resources.some((resource) => resource.type === 'file' && resource.file_id === file.id),
      `${version}: Files resource`,
    );
    const metadata = await client.beta.files.retrieveMetadata(file.id, { betas: FILE_BETAS });
    assert.equal(metadata.downloadable, false, `${version}: uploaded Files stay input-only`);
    await assert.rejects(
      () => client.beta.files.download(file.id, { betas: FILE_BETAS }),
      (error) => error?.status === 400,
      `${version}: input File download fails closed`,
    );

    await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `cloud-sdk-matrix-${version}` }],
      }],
      betas: BETAS,
    });
    const events = await eventsUntilIdle(client, session.id);
    assert.ok(events.some((event) => event.type === 'agent.message'), `${version}: agent reply`);
    const archived = await client.beta.sessions.archive(session.id, { betas: BETAS });
    assert.ok(archived.archived_at, `${version}: archive`);
  } finally {
    if (session) await client.beta.sessions.delete(session.id, { betas: BETAS });
    await client.beta.files.delete(file.id, { betas: FILE_BETAS });
  }
}

async function exerciseVersionSelectionBoundary(oldest, current) {
  const request = (headers = {}) => fetch(`${baseURL}/v1/sessions`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'x-api-key': apiKey,
      'anthropic-version': '2023-06-01',
      ...headers,
    },
    body: JSON.stringify({ agent, environment_id: environmentId }),
  });
  const oldTelemetry = await request({
    'anthropic-beta': BETAS[0],
    'user-agent': `anthropic-sdk-typescript/${oldest.version}`,
    'x-stainless-package-version': oldest.version,
  });
  const newTelemetry = await request({
    'anthropic-beta': BETAS[0],
    'user-agent': `anthropic-sdk-typescript/${current.version}`,
    'x-stainless-package-version': current.version,
  });
  assert.equal(oldTelemetry.status, 200, 'old telemetry reaches the canonical route');
  assert.equal(newTelemetry.status, 200, 'new telemetry reaches the canonical route');
  const created = [await oldTelemetry.json(), await newTelemetry.json()];
  assert.deepEqual(
    Object.keys(created[0]).sort(),
    Object.keys(created[1]).sort(),
    'telemetry differences do not select another response schema',
  );

  const absentBeta = await request({
    'user-agent': `anthropic-sdk-typescript/${current.version}`,
    'x-awaken-managed-version': current.version,
  });
  assert.equal(absentBeta.status, 400, 'private version headers cannot grant Managed access');
  const wrongBeta = await request({ 'anthropic-beta': 'managed-agents-private-test' });
  assert.equal(wrongBeta.status, 400, 'unknown beta fails closed');
  const badCursor = await fetch(`${baseURL}/v1/sessions?limit=1&page=not-a-cursor`, {
    headers: {
      'x-api-key': apiKey,
      'anthropic-version': '2023-06-01',
      'anthropic-beta': BETAS[0],
    },
  });
  assert.equal(badCursor.status, 400, 'fabricated pagination cursor fails closed');

  for (const value of created) {
    await fetch(`${baseURL}/v1/sessions/${value.id}`, {
      method: 'DELETE',
      headers: {
        'x-api-key': apiKey,
        'anthropic-version': '2023-06-01',
        'anthropic-beta': BETAS[0],
      },
    });
  }
}

async function exerciseTunnelPublicLifecycle(CurrentClient) {
  const apiKeyAttempt = await fetch(`${baseURL}/v1/tunnels`, {
    headers: {
      'x-api-key': apiKey,
      'anthropic-version': '2023-06-01',
      'anthropic-beta': TUNNEL_BETAS[0],
    },
  });
  assert.ok(
    [401, 403].includes(apiKeyAttempt.status),
    `Managed API keys cannot access Tunnel endpoints: ${apiKeyAttempt.status}`,
  );
  const malformedAuthorization = await fetch(`${baseURL}/v1/tunnels`, {
    headers: {
      authorization: 'Basic invalid',
      'x-api-key': apiKey,
      'anthropic-version': '2023-06-01',
      'anthropic-beta': TUNNEL_BETAS[0],
    },
  });
  assert.equal(
    malformedAuthorization.status,
    401,
    'malformed Authorization cannot fall back to x-api-key',
  );

  const client = new CurrentClient({ authToken: tunnelAccessToken, baseURL });
  let tunnel;
  let certificate;
  try {
    tunnel = await client.beta.tunnels.create({
      display_name: `release-${Date.now()}`,
      betas: TUNNEL_BETAS,
    });
    assert.equal((await client.beta.tunnels.retrieve(tunnel.id, { betas: TUNNEL_BETAS })).id, tunnel.id);
    assert.ok(
      (await drain(client.beta.tunnels.list({ betas: TUNNEL_BETAS })))
        .some((candidate) => candidate.id === tunnel.id),
      'created Tunnel is visible through public ingress',
    );
    const legacyList = await fetch(`${baseURL}/v1/organizations/tunnels`, {
      headers: {
        'x-api-key': apiKey,
        'anthropic-version': '2023-06-01',
        'anthropic-beta': LEGACY_TUNNEL_BETA,
      },
    });
    assert.equal(legacyList.status, 200, 'legacy Admin API remains available during migration');
    assert.ok(
      (await legacyList.json()).data.some((candidate) => candidate.id === tunnel.id),
      'legacy and current routes project the same Tunnel aggregate',
    );
    const wrongLegacyBeta = await fetch(`${baseURL}/v1/organizations/tunnels`, {
      headers: {
        'x-api-key': apiKey,
        'anthropic-version': '2023-06-01',
        'anthropic-beta': TUNNEL_BETAS[0],
      },
    });
    assert.equal(wrongLegacyBeta.status, 400, 'current beta cannot select legacy auth semantics');
    const wifOnLegacy = await fetch(`${baseURL}/v1/organizations/tunnels`, {
      headers: {
        authorization: `Bearer ${tunnelAccessToken}`,
        'anthropic-version': '2023-06-01',
        'anthropic-beta': LEGACY_TUNNEL_BETA,
      },
    });
    assert.equal(wifOnLegacy.status, 403, 'WIF token cannot select legacy Admin API semantics');
    assert.ok((await client.beta.tunnels.revealToken(tunnel.id, { betas: TUNNEL_BETAS })).tunnel_token);
    assert.ok((await client.beta.tunnels.rotateToken(tunnel.id, {
      reason: 'controlled-staging release evidence', betas: TUNNEL_BETAS,
    })).tunnel_token);

    const beforeInvalid = await drain(client.beta.tunnels.certificates.list(tunnel.id, {
      betas: TUNNEL_BETAS,
    }));
    await assert.rejects(
      () => client.beta.tunnels.certificates.create(tunnel.id, {
        ca_certificate_pem: 'not a certificate',
        betas: TUNNEL_BETAS,
      }),
      (error) => error?.status === 400,
      'invalid CA material fails closed',
    );
    assert.equal(
      (await drain(client.beta.tunnels.certificates.list(tunnel.id, {
        betas: TUNNEL_BETAS,
      }))).length,
      beforeInvalid.length,
      'invalid certificate registration has no side effect',
    );
    certificate = await client.beta.tunnels.certificates.create(tunnel.id, {
      ca_certificate_pem: TUNNEL_CA_PEM,
      betas: TUNNEL_BETAS,
    });
    assert.equal(certificate.type, 'tunnel_certificate');
    assert.equal(certificate.tunnel_id, tunnel.id);
    assert.match(certificate.fingerprint, /^[0-9a-f]{64}$/);
    assert.equal((await client.beta.tunnels.certificates.retrieve(certificate.id, {
      tunnel_id: tunnel.id,
      betas: TUNNEL_BETAS,
    })).id, certificate.id);
    assert.ok(
      (await drain(client.beta.tunnels.certificates.list(tunnel.id, {
        betas: TUNNEL_BETAS,
      }))).some((candidate) => candidate.id === certificate.id),
      'active certificate is listed',
    );
    await assert.rejects(
      () => client.beta.tunnels.certificates.retrieve('tcrt_unknown', {
        tunnel_id: tunnel.id,
        betas: TUNNEL_BETAS,
      }),
      (error) => error?.status === 404,
      'unknown nested certificate is 404',
    );
    const archivedCertificate = await client.beta.tunnels.certificates.archive(certificate.id, {
      tunnel_id: tunnel.id,
      betas: TUNNEL_BETAS,
    });
    assert.ok(archivedCertificate.archived_at);
    assert.ok(
      !(await drain(client.beta.tunnels.certificates.list(tunnel.id, {
        betas: TUNNEL_BETAS,
      }))).some((candidate) => candidate.id === certificate.id),
      'default certificate list excludes archived records',
    );
    assert.ok(
      (await drain(client.beta.tunnels.certificates.list(tunnel.id, {
        include_archived: true,
        betas: TUNNEL_BETAS,
      }))).some((candidate) => candidate.id === certificate.id && candidate.archived_at),
      'include_archived restores the nested record',
    );
    assert.equal(
      (await client.beta.tunnels.certificates.archive(certificate.id, {
        tunnel_id: tunnel.id,
        betas: TUNNEL_BETAS,
      })).archived_at,
      archivedCertificate.archived_at,
      'certificate archive is idempotent',
    );
  } finally {
    if (tunnel) {
      const archived = await client.beta.tunnels.archive(tunnel.id, { betas: TUNNEL_BETAS });
      assert.ok(archived.archived_at, 'release evidence never leaves an active Tunnel');
      assert.ok(
        (await client.beta.tunnels.retrieve(tunnel.id, { betas: TUNNEL_BETAS })).archived_at,
        'archived Tunnel remains auditable',
      );
      await assert.rejects(
        () => client.beta.tunnels.revealToken(tunnel.id, { betas: TUNNEL_BETAS }),
        (error) => error?.status === 409,
        'archived Tunnel token cannot be revealed',
      );
    }
  }
}

// Cause/effect graph: oldest supported and current SDK -> identical hosted
// public ingress/beta -> canonical Awaken Session lifecycle. Decision table:
// supported version succeeds; missing beta is rejected by the shared router;
// User-Agent/x-stainless differences never select another implementation.
const oldest = qualifiedClient(clients, 'oldest_supported');
const userProfilesLegacy = qualifiedClient(clients, 'protocol_change_point');
const current = qualifiedClient(clients, 'current_oracle');
await exerciseVersionSelectionBoundary(oldest, current);
await exerciseUserProfileChangePoint({
  profileID: userProfileId,
  expectedAccessType: userProfileAccessType,
  legacyClient: new userProfilesLegacy.Client({ apiKey, baseURL }),
  currentClient: new current.Client({ apiKey, baseURL }),
});
await exerciseIngressHeaderFidelity();
for (const client of clients) await exercise(client.version, client.Client, client.toFile);
await exerciseTunnelPublicLifecycle(current.Client);
console.log(
  `Managed public-ingress SDK matrix passed for ${clients.map(({ version }) => version).join(', ')}, including the User Profiles change point, repeated/scoped beta headers, negative routing, Files/Resources, and the WIF-only Tunnel/Certificate lifecycle`,
);
