// Provider model-directory discovery through the public management API.
//
// Discovery is provisioning input only: the adapter reads one authored endpoint
// and one exact Workspace credential, returns a complete secret-free listing, and
// the catalog reconciles it atomically. Exercise every supported provider dialect
// and the fail-closed response boundaries without making a live provider call.

import assert from 'node:assert/strict';
import http from 'node:http';
import { withScenarioServer } from './harness.mjs';

const WORKSPACE = `discovery-workspace-${process.pid}`;
const PROVIDER = `discovery-provider-${process.pid}`;
const KEY = `sk-discovery-${process.pid}`; // awaken-allow: secret

let requestSequence = 0;
async function request(base, method, uri, body) {
  requestSequence += 1;
  const response = await fetch(`${base}${uri}`, {
    method,
    headers: body === undefined ? {} : {
      'content-type': 'application/json',
      'x-request-id': `discovery-${process.pid}-${requestSequence}`,
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let parsed = null;
  try {
    parsed = text ? JSON.parse(text) : null;
  } catch {
    parsed = text;
  }
  return {
    status: response.status,
    body: parsed,
  };
}

async function directoryServer() {
  const seen = [];
  const server = http.createServer((incoming, response) => {
    const url = new URL(incoming.url, 'http://fixture.invalid');
    seen.push({
      path: url.pathname,
      query: Object.fromEntries(url.searchParams),
      authorization: incoming.headers.authorization,
      apiKey: incoming.headers['x-api-key'],
      anthropicVersion: incoming.headers['anthropic-version'],
    });
    const route = url.pathname.split('/')[1];

    const json = (status, body) => {
      response.writeHead(status, { 'content-type': 'application/json' });
      response.end(JSON.stringify(body));
    };
    if (route === 'http-error') return json(503, { error: 'unavailable' });
    if (route === 'invalid-json') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end('{not-json');
      return;
    }
    if (route === 'missing-array') return json(200, {});
    if (route === 'missing-id') return json(200, { data: [{}], has_more: false });
    if (route === 'missing-cursor') {
      return json(200, { data: [{ id: 'cursorless' }], has_more: true });
    }
    if (route === 'repeat-cursor') {
      return json(200, {
        data: [{ id: url.searchParams.has('after_id') ? 'repeat-b' : 'repeat-a' }],
        has_more: true,
        last_id: 'same-cursor',
      });
    }
    if (route === 'anthropic-page') {
      if (url.searchParams.get('after_id') === 'cursor-1') {
        return json(200, {
          data: [{ id: 'anthropic-duplicate' }, { id: 'anthropic-b' }],
          has_more: false,
        });
      }
      return json(200, {
        data: [{ id: 'anthropic-a' }, { id: 'anthropic-duplicate' }],
        has_more: true,
        last_id: 'cursor-1',
      });
    }
    if (route === 'openai') {
      return json(200, { data: [{ id: 'openai-a' }, { id: 'openai-b' }] });
    }
    if (route === 'gemini') {
      if (url.searchParams.get('pageToken') === 'gemini-next') {
        return json(200, { models: [{ name: 'models/gemini-b' }] });
      }
      return json(200, {
        models: [{ name: 'models/gemini-a' }],
        nextPageToken: 'gemini-next', // awaken-allow: secret
      });
    }
    if (route === 'vertex') {
      return json(200, { models: [{ name: 'vertex-a' }] });
    }
    return json(404, { error: 'unknown fixture route' });
  });
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === 'object');
  return {
    seen,
    url: `http://127.0.0.1:${address.port}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

async function main() {
  const directory = await directoryServer();
  try {
    await withScenarioServer('management', 'mcp', 39414, async (base) => {
      let result = await request(base, 'PUT', `/v1/config/providers/${PROVIDER}`, {
        id: 'path-is-authoritative',
        slug: PROVIDER,
        display_name: 'Discovery fixture',
        version: 1,
      });
      assert.equal(result.status, 200, JSON.stringify(result.body));

      result = await request(base, 'POST', '/v1/config/credentials', {
        workspace_id: WORKSPACE,
        kind: 'vault',
        provider_id: PROVIDER,
        secret: KEY,
      });
      assert.equal(result.status, 201, JSON.stringify(result.body));
      const credentialId = result.body.id;

      const putEndpoint = async (id, dialect, baseUrl) => {
        const response = await request(base, 'PUT', `/v1/config/endpoints/${id}`, {
          id,
          provider_id: PROVIDER,
          dialect,
          base_url: baseUrl,
          timeout_secs: 30,
          display_name: id,
          version: 1,
        });
        assert.equal(response.status, 200, `${id}: ${JSON.stringify(response.body)}`);
      };
      const discover = (id, body = {
        workspace_id: WORKSPACE,
        credential_source_id: credentialId,
      }) => request(base, 'POST', `/v1/config/endpoints/${id}/discover-models`, body);

      await putEndpoint('anthropic-page', 'anthropic_messages', `${directory.url}/anthropic-page/v1/`);
      result = await discover('anthropic-page');
      assert.equal(result.status, 200, JSON.stringify(result.body));
      assert.equal(result.body.discovered, 3);
      const anthropicRequests = directory.seen.filter((entry) =>
        entry.path.startsWith('/anthropic-page/'));
      assert.equal(anthropicRequests.length, 2);
      assert.equal(anthropicRequests[0].apiKey, KEY);
      assert.equal(anthropicRequests[0].anthropicVersion, '2023-06-01');
      assert.equal(anthropicRequests[1].query.after_id, 'cursor-1');

      await putEndpoint('openai', 'open_ai_chat', `${directory.url}/openai/v1/`);
      result = await discover('openai');
      assert.equal(result.status, 200, JSON.stringify(result.body));
      assert.equal(result.body.discovered, 2);
      assert.equal(
        directory.seen.find((entry) => entry.path.startsWith('/openai/')).authorization,
        `Bearer ${KEY}`,
      );

      await putEndpoint('gemini', 'gemini', `${directory.url}/gemini/v1beta/`);
      result = await discover('gemini');
      assert.equal(result.status, 200, JSON.stringify(result.body));
      assert.equal(result.body.discovered, 2);
      const geminiRequests = directory.seen.filter((entry) => entry.path.startsWith('/gemini/'));
      assert.equal(geminiRequests.length, 2);
      assert.equal(geminiRequests[0].query.key, KEY);
      assert.equal(geminiRequests[1].query.pageToken, 'gemini-next');

      await putEndpoint(
        'vertex',
        'vertex_gemini',
        `${directory.url}/vertex/v1/projects/p/locations/l/`,
      );
      result = await discover('vertex');
      assert.equal(result.status, 200, JSON.stringify(result.body));
      const vertexRequest = directory.seen.find((entry) => entry.path.startsWith('/vertex/'));
      assert.match(vertexRequest.path, /publishers\/google\/models$/);
      assert.equal(vertexRequest.authorization, `Bearer ${KEY}`);

      result = await discover('anthropic-page', { credential_source_id: credentialId });
      assert.equal(result.status, 200, JSON.stringify(result.body));

      result = await discover('anthropic-page', {
        workspace_id: `other-${WORKSPACE}`,
        credential_source_id: credentialId,
      });
      assert.equal(result.status, 200, JSON.stringify(result.body));

      result = await discover('missing-endpoint');
      assert.equal(result.status, 404);
      result = await discover('anthropic-page', {
        workspace_id: WORKSPACE,
        credential_source_id: 'cred_missing',
      });
      assert.equal(result.status, 404);

      for (const route of [
        'http-error',
        'invalid-json',
        'missing-array',
        'missing-id',
        'missing-cursor',
        'repeat-cursor',
      ]) {
        await putEndpoint(route, 'anthropic_messages', `${directory.url}/${route}/v1/`);
        result = await discover(route);
        assert.equal(result.status, 502, `${route}: ${JSON.stringify(result.body)}`);
        assert.equal(result.body.code, 'model_discovery_failed');
      }

      await putEndpoint('invalid-url', 'anthropic_messages', 'not a valid URL');
      result = await discover('invalid-url');
      assert.equal(result.status, 502, JSON.stringify(result.body));

      await putEndpoint('vertex-no-base', 'vertex_gemini', null);
      result = await discover('vertex-no-base');
      assert.equal(result.status, 502, JSON.stringify(result.body));

      result = await request(base, 'POST', `/v1/config/credentials/${credentialId}/archive`, {});
      assert.equal(result.status, 200, JSON.stringify(result.body));
      result = await discover('anthropic-page');
      assert.equal(result.status, 409);
      assert.equal(result.body.code, 'credential_inactive');

      console.log(
        'MODEL DISCOVERY TS E2E PASS: all supported dialects reconcile complete directories and transport, codec, cursor, scope, and credential failures close before catalog mutation.',
      );
    });
  } finally {
    await directory.close();
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
