// Awaken Cloud identity over the production awaken-iam remote protocol. The
// fixture is an IAM service boundary (JWKS + PDP), not an awaken test endpoint:
// the spawned product still performs real JWT verification, workspace routing,
// PEP enforcement, and remote authorization over HTTP.
//
// Brokered model cause graph / decision table:
// C1 valid cached Cloud login, C2 Cloud readiness, C3 native model projection,
// C4 explicit brokered Profile binding, C5 current attempt ownership, C6 Cloud
// grant, C7 native Gateway function call + tool result + final response,
// C8 later model removal, C9 transient
// catalog transport failure.
// T1 C1+C2+C3 -> active brokered Offering + Cloud-provenance token metadata.
// T2 C1+C2+C3+C4 -> exact credential-free publication preview (never `none`).
// T3 C1+C2+C3+C4+C5+C6+C7 -> each pre/post-action attempt obtains and closes
// its own grant; tool schema/call/result cross two Responses calls and no
// Provider key exists.
// T4 C1+C2+C8 -> Offering unavailable and only stale brokered metadata removed.
// T5 C1+C9 -> bounded retry of the idempotent catalog snapshot only; grant and
// usage paths are never replayed (the retry decision table is unit tested).
// T6 C1+C3+C4+C5 + each Cloud grant rejection -> one terminal Session error
// with no Gateway call and no accidental grant close.
// T7 a valid grant + Gateway HTTP failure/invalid native payload -> one terminal
// Session error and the issued grant is still closed.
// T8 a valid grant + text/base64 image -> native Responses input preserves both
// blocks; incomplete and unknown output variants remain valid terminal responses.
// T9 malformed/unsupported Cloud projections fail atomically; duplicate public
// model observations merge only the conservative known attribute minimum.
// T10 Cloud login on + Cloud models off -> identity remains authenticated while
// Catalog refresh fails locally and performs zero Cloud inference requests.

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { generateKeyPairSync, sign } from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { deploymentEnv, spawnServer, stopServer, waitForPort, pass } from './harness.mjs';

const PORT = 38257;
const ISSUER = 'https://accounts.e2e.awakenworks.test';
const AUDIENCE = 'awaken-runtime';
const KEY_ID = 'cloud-e2e-ed25519';
const SERVICE_TOKEN = 'cloud-e2e-service-credential'; // awaken-allow: secret
const SEAL_KEY = '12233445566778899aabbccddeeff00112233445566778899aabbccddeeff001';

function b64url(value) {
  return Buffer.from(value).toString('base64url');
}

function accessToken(privateKey, subject, { expiresIn = 3600, audience = AUDIENCE } = {}) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: 'EdDSA', typ: 'JWT', kid: KEY_ID }));
  const payload = b64url(JSON.stringify({
    iss: ISSUER,
    sub: subject,
    aud: audience,
    exp: now + expiresIn,
    iat: now,
    jti: `jti-${subject}-${expiresIn}`,
  }));
  const input = `${header}.${payload}`;
  return `${input}.${sign(null, Buffer.from(input), privateKey).toString('base64url')}`;
}

async function startIamFixture() {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519');
  const publicJwk = publicKey.export({ format: 'jwk' });
  const calls = [];
  const cloudCalls = [];
  const grantCalls = [];
  const gatewayCalls = [];
  let grantFailure = null;
  const gatewayResponses = [];
  let fixtureUrl = '';
  let decision = 'allow';
  const readinessFailures = [];
  let cloudModels = [{
    provider: 'openai',
    original_model_id: 'gpt-5-e2e',
    native_protocol: 'openai_responses',
    context_window: 400000,
    max_output_tokens: 128000,
    capabilities: ['responses'],
    route_publication_revision: 7,
  }];
  const server = http.createServer(async (request, response) => {
    if (request.method === 'GET' && request.url === '/.well-known/jwks.json') {
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({
        keys: [{
          kty: 'OKP', crv: 'Ed25519', x: publicJwk.x, kid: KEY_ID, use: 'sig', alg: 'EdDSA',
        }],
      }));
      return;
    }
    if (request.method === 'GET' && request.url === '/v1/inference/readiness') {
      const failure = readinessFailures.shift();
      cloudCalls.push({
        path: request.url,
        authorization: request.headers.authorization,
        injected_failure: failure?.code,
      });
      if (failure) {
        response.statusCode = failure.status;
        response.setHeader('content-type', 'application/json');
        response.end(JSON.stringify({ error: failure.code }));
        return;
      }
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({ ready: true }));
      return;
    }
    if (request.method === 'GET' && request.url === '/v1/inference/models') {
      cloudCalls.push({ path: request.url, authorization: request.headers.authorization });
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({ data: cloudModels }));
      return;
    }
    if (request.method === 'GET' && request.url === '/v1/models') {
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({ data: [{ id: 'direct-publication-e2e' }] }));
      return;
    }
    if (request.method === 'POST' && request.url === '/v1/inference/grants') {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      const grantId = `grant-${grantCalls.length + 1}`;
      grantCalls.push({
        kind: 'create',
        body,
        authorization: request.headers.authorization,
        idempotencyKey: request.headers['idempotency-key'],
        grantId,
      });
      const failure = grantFailure?.remaining > 0 ? grantFailure : null;
      if (failure) {
        failure.remaining -= 1;
        response.statusCode = failure.status;
        if (failure.retryAfter !== undefined) response.setHeader('retry-after', failure.retryAfter);
        response.setHeader('content-type', 'application/json');
        response.end(JSON.stringify({ error: failure.code }));
        return;
      }
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({
        grant_id: grantId,
        gateway_base_url: `${fixtureUrl}/v1`,
        capability: `capability-${grantId}`,
        grant_expires_at: Math.floor(Date.now() / 1000) + 60,
      }));
      return;
    }
    const close = request.url.match(/^\/v1\/inference\/grants\/([^/]+)\/close$/u);
    if (request.method === 'POST' && close) {
      grantCalls.push({
        kind: 'close',
        grantId: close[1],
        authorization: request.headers.authorization,
      });
      response.statusCode = 204;
      response.end();
      return;
    }
    if (request.method === 'POST' && request.url === '/v1/responses') {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      gatewayCalls.push({ body, authorization: request.headers.authorization });
      const queued = gatewayResponses.shift();
      if (queued) {
        response.statusCode = queued.status ?? 200;
        response.setHeader('content-type', 'application/json');
        response.end(typeof queued.body === 'string' ? queued.body : JSON.stringify(queued.body));
        return;
      }
      const hasToolResult = body.input.some((item) => item.type === 'function_call_output');
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify(hasToolResult
        ? {
            status: 'completed',
            output: [{
              type: 'message',
              content: [{ type: 'output_text', text: 'BROKERED-RESPONSES-E2E' }],
            }],
            usage: { input_tokens: 11, output_tokens: 3 },
          }
        : {
            status: 'completed',
            output: [{
              type: 'function_call',
              call_id: 'call-brokered-custom',
              name: 'cloud_echo',
              arguments: JSON.stringify({ value: 'CLOUD-TOOL-E2E' }),
            }],
            usage: { input_tokens: 7, output_tokens: 2 },
          }));
      return;
    }
    if (request.method === 'POST' && request.url === '/v1/authorize') {
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
      calls.push({
        body,
        authorization: request.headers.authorization,
        audience: request.headers['x-iam-audience'],
      });
      response.setHeader('content-type', 'application/json');
      response.end(JSON.stringify({
        decision,
        reason: `e2e_${decision}`,
        matched_grants: decision === 'allow' ? ['grant-e2e'] : [],
        matched_roles: decision === 'allow' ? ['role-e2e'] : [],
      }));
      return;
    }
    response.statusCode = 404;
    response.end();
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const address = server.address();
  fixtureUrl = `http://127.0.0.1:${address.port}`;
  return {
    url: fixtureUrl,
    token: (subject, options) => accessToken(privateKey, subject, options),
    calls,
    cloudCalls,
    grantCalls,
    gatewayCalls,
    failGrants(status, code, retryAfter, remaining = 1) {
      grantFailure = { status, code, retryAfter, remaining };
    },
    clearGrantFailures() { grantFailure = null; },
    respondNextGateway(body, status = 200, remaining = 1) {
      for (let index = 0; index < remaining; index += 1) {
        gatewayResponses.push({ status, body });
      }
    },
    clearGatewayResponses() { gatewayResponses.length = 0; },
    decide(value) { decision = value; },
    failNextReadiness(status, code) { readinessFailures.push({ status, code }); },
    setCloudModels(value) { cloudModels = value; },
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

async function req(base, method, uri, token, { apiKey = false, body } = {}) {
  const headers = {};
  if (token) headers[apiKey ? 'x-api-key' : 'authorization'] = apiKey ? token : `Bearer ${token}`;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const response = await fetch(`${base}${uri}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, body: text ? JSON.parse(text) : null };
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-cloud-iam-e2e-'));
  const iam = await startIamFixture();
  const cachedToken = iam.token('account-cached');
  const explicitToken = iam.token('account-explicit');
  const expiredToken = iam.token('account-expired', { expiresIn: -1 });
  const wrongAudienceToken = iam.token('account-wrong-audience', { audience: 'other-service' });
  let server = null;
  try {
    const localOnlyDirectory = path.join(directory, 'cloud-login-local-models');
    const localOnlyEnv = deploymentEnv(localOnlyDirectory, {
      identityMode: 'awaken-cloud',
      controlSealKey: SEAL_KEY,
      fields: { cloud_api_url: iam.url, cloud_models: 'disabled' },
      cloudIam: {
        url: iam.url,
        issuer: ISSUER,
        audience: AUDIENCE,
        accessToken: cachedToken,
        serviceToken: SERVICE_TOKEN,
      },
    });
    const cloudCallsBeforeDisabledBoot = iam.cloudCalls.length;
    ({ server } = spawnServer('management-providers', PORT, localOnlyEnv));
    await waitForPort(PORT, 180_000, server);
    let localOnlyResult = await req(
      `http://127.0.0.1:${PORT}`,
      'GET',
      '/v1/config/capabilities',
    );
    assert.equal(localOnlyResult.status, 200, JSON.stringify(localOnlyResult.body));
    assert.deepEqual(localOnlyResult.body.identity, {
      mode: 'awaken-cloud',
      cloud_login_enabled: true,
      authenticated: true,
    });
    assert.equal(localOnlyResult.body.models.cloud_models_enabled, false);
    localOnlyResult = await req(
      `http://127.0.0.1:${PORT}`,
      'POST',
      '/v1/config/brokered-models/refresh',
    );
    assert.equal(localOnlyResult.status, 409, JSON.stringify(localOnlyResult.body));
    assert.equal(localOnlyResult.body.code, 'cloud_models_disabled');
    assert.equal(
      iam.cloudCalls.length,
      cloudCallsBeforeDisabledBoot,
      'disabled Cloud model supply must not call readiness or catalog APIs',
    );
    await stopServer(server);
    server = null;
    pass('Cloud login and Cloud model supply are independent; disabled supply has zero Cloud traffic');

    const env = deploymentEnv(directory, {
      identityMode: 'awaken-cloud',
      controlSealKey: SEAL_KEY,
      fields: { cloud_api_url: iam.url, cloud_models: 'enabled' },
      cloudIam: {
        url: iam.url,
        issuer: ISSUER,
        audience: AUDIENCE,
        accessToken: cachedToken,
        serviceToken: SERVICE_TOKEN,
      },
    });
    ({ server } = spawnServer('management-providers', PORT, env));
    await waitForPort(PORT, 180_000, server);
    const base = `http://127.0.0.1:${PORT}`;
    const localWorkspace = fs.readFileSync(path.join(directory, 'platform-workspace-id'), 'utf8').trim();

    // No request credential means "use the cloud login cached by this local
    // process". The platform workspace is resolved at the composition edge.
    let result = await req(base, 'GET', '/v1/files');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    let call = iam.calls.at(-1);
    assert.deepEqual(call.body.principal, { kind: 'account', account_id: 'account-cached' });
    assert.deepEqual(call.body.scope, { kind: 'workspace', workspace_id: localWorkspace });
    assert.equal(call.authorization, `Bearer ${SERVICE_TOKEN}`);
    assert.equal(call.audience, AUDIENCE);
    pass('cached cloud login -> resource PEP -> remote PDP with platform workspace');

    // An explicit Bearer or x-api-key overrides the cached login. Workspace path
    // selection is trusted only after the edge rewrite and reaches the PDP as the
    // target; the inner resource service sees only the stamped WorkspaceScope.
    const selectedWorkspace = 'workspace-cloud-selected';
    result = await req(base, 'GET', `/v1/workspaces/${selectedWorkspace}/skills`, explicitToken);
    assert.equal(result.status, 200, JSON.stringify(result.body));
    call = iam.calls.at(-1);
    assert.deepEqual(call.body.principal, { kind: 'account', account_id: 'account-explicit' });
    assert.equal(call.body.action, 'awaken.runtime.resources::skill.read');
    assert.deepEqual(call.body.scope, { kind: 'workspace', workspace_id: selectedWorkspace });
    result = await req(base, 'GET', `/v1/workspaces/${selectedWorkspace}/memory_stores`, explicitToken, { apiKey: true });
    assert.equal(result.status, 200, JSON.stringify(result.body));
    pass('explicit Bearer/x-api-key and workspace path use the same cloud PEP');

    result = await req(base, 'GET', `/v1/workspaces/${selectedWorkspace}/config/catalog`, explicitToken);
    assert.equal(result.status, 200, JSON.stringify(result.body));
    call = iam.calls.at(-1);
    assert.equal(call.body.action, 'awaken.runtime.management::workspace.read');
    assert.deepEqual(call.body.scope, { kind: 'workspace', workspace_id: selectedWorkspace });
    pass('management PEP uses the same remote IAM protocol and trusted workspace');

    result = await req(base, 'POST', '/v1/config/brokered-models/refresh');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    assert.equal(result.body.activated, 1);
    assert.deepEqual(
      iam.cloudCalls.map((entry) => entry.path),
      ['/v1/inference/readiness', '/v1/inference/models'],
    );
    assert.ok(iam.cloudCalls.every((entry) => entry.authorization === `Bearer ${cachedToken}`));
    result = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    const brokered = result.body.offerings.find((offering) => offering.source === 'brokered');
    assert.ok(brokered, JSON.stringify(result.body));
    assert.equal(brokered.model_id, 'gpt-5-e2e');
    assert.equal(result.body.model_attributes['gpt-5-e2e'].context_window, 400000);
    assert.equal(
      result.body.model_attributes['gpt-5-e2e'].provenance.context_window.source,
      'brokered',
    );

    result = await req(base, 'PUT', '/v1/config/inference-profiles/publication-route', cachedToken, {
      body: {
        workspace_id: localWorkspace,
        primary: {
          target: {
            model_id: brokered.model_id,
            provider_id: brokered.provider_id,
            protocol_endpoint_id: brokered.protocol_endpoint_id,
          },
          credential_binding: { type: 'brokered' },
        },
        fallbacks: [],
        disabled_endpoint_ids: [],
      },
    });
    assert.equal(result.status, 200, JSON.stringify(result.body));
    result = await req(
      base,
      'POST',
      '/v1/config/inference-profiles/publication-route/resolve-candidates',
      cachedToken,
      { body: { workspace_id: localWorkspace } },
    );
    assert.equal(result.status, 200, JSON.stringify(result.body));
    const candidate = result.body.candidates[0];
    assert.equal(candidate.model_id, 'gpt-5-e2e');
    assert.equal(candidate.credential_present, false);
    pass('Cloud model refresh and explicit brokered Profile preserve exact public identity');

    // Publication-access cause graph: C1 offering is brokered; C2 binding is
    // brokered; C3 exact local credential is active+compatible; C4 selected pool
    // has an active compatible member; C5 profile target resolves uniquely.
    // Saving a profile is intentionally not proof that it can be frozen into an
    // executable publication, so every row crosses the real publish boundary.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5 | Expected |
    // | B1   | Y  | N  | -  | -  | Y  | 409 explicit brokered binding required |
    // | B2   | N  | Y  | -  | -  | Y  | 409 brokered binding rejected          |
    // | B3   | N  | N  | N  | -  | Y  | 409 exact credential rejected          |
    // | B4   | N  | N  | -  | N  | Y  | 409 pool exhausted                     |
    // | B5   | N  | N  | -  | Y  | Y  | 200 pool member frozen                 |
    // | B6   | N  | N  | -  | -  | Y  | 200 credential-free direct candidate  |
    // | B7   | N  | N  | -  | -  | N  | 409 qualified endpoint required       |
    const directTarget = {
      model_id: 'direct-publication-e2e',
      provider_id: 'openai',
      protocol_endpoint_id: 'direct-e2e-primary',
    };
    result = await req(base, 'POST', '/v1/config/provider-connections', cachedToken, {
      body: {
        workspace_id: localWorkspace,
        provider_id: 'openai',
        display_name: 'Direct E2E',
        endpoint_id: 'direct-e2e-primary',
        dialect: 'open_ai_chat',
        base_url: `${iam.url}/v1/`,
        timeout_secs: 30,
        secret: 'sk-cloud-byok-publication-e2e', // awaken-allow: secret
      },
    });
    assert.equal(result.status, 201, JSON.stringify(result.body));
    const directCredentialId = result.body.credential.id;
    for (const [poolId, members] of [
      ['publication-empty-pool', []],
      ['publication-ready-pool', [{
        credential_source_id: directCredentialId,
        ordinal: 0,
        enabled: true,
        selection_weight: 1,
      }]],
    ]) {
      result = await req(base, 'PUT', `/v1/config/credential-pools/${poolId}`, cachedToken, {
        body: { id: poolId, workspace_id: localWorkspace, members },
      });
      assert.equal(result.status, 200, JSON.stringify(result.body));
    }

    const publicationMatrixAgent = 'publication-access-matrix-agent';
    result = await req(base, 'PUT', `/v1/config/agents/${publicationMatrixAgent}`, cachedToken, {
      body: {
        id: publicationMatrixAgent,
        name: 'Publication Access Matrix',
        system: 'Freeze exactly the configured model access.',
        max_steps: 2,
        model: { mode: 'auto' },
        tools: [],
      },
    });
    assert.equal(result.status, 200, JSON.stringify(result.body));
    const putDefaultProfile = async (target, credentialBinding) => {
      const saved = await req(
        base,
        'PUT',
        '/v1/config/inference-profiles/publication-route',
        cachedToken,
        {
          body: {
            workspace_id: localWorkspace,
            primary: { target, credential_binding: credentialBinding },
            fallbacks: [],
            disabled_endpoint_ids: [],
          },
        },
      );
      assert.equal(saved.status, 200, JSON.stringify(saved.body));
    };
    const publishMatrixAgent = () => req(
      base,
      'POST',
      `/v1/config/agents/${publicationMatrixAgent}/publish`,
      cachedToken,
    );
    const rejectedPublication = async (target, binding, message) => {
      await putDefaultProfile(target, binding);
      const rejected = await publishMatrixAgent();
      assert.equal(rejected.status, 409, JSON.stringify(rejected.body));
      assert.match(rejected.body.error, message);
    };

    await rejectedPublication(
      {
        model_id: brokered.model_id,
        provider_id: brokered.provider_id,
        protocol_endpoint_id: brokered.protocol_endpoint_id,
      },
      { type: 'none' },
      /explicit brokered access binding/u,
    );
    await rejectedPublication(
      directTarget,
      { type: 'brokered' },
      /requires a brokered catalog offering/u,
    );
    await rejectedPublication(
      directTarget,
      { type: 'exact', credential_source_id: 'missing-direct-credential' },
      /absent, inactive, or incompatible/u,
    );
    await rejectedPublication(
      directTarget,
      { type: 'one_of_credential_pool', credential_pool_id: 'publication-empty-pool' },
      /has no active compatible member/u,
    );

    await putDefaultProfile(directTarget, {
      type: 'one_of_credential_pool', credential_pool_id: 'publication-ready-pool',
    });
    result = await publishMatrixAgent();
    assert.equal(result.status, 200, JSON.stringify(result.body));
    await putDefaultProfile(directTarget, { type: 'none' });
    result = await publishMatrixAgent();
    assert.equal(result.status, 200, JSON.stringify(result.body));

    result = await req(base, 'POST', '/v1/config/provider-connections', cachedToken, {
      body: {
        workspace_id: localWorkspace,
        provider_id: 'openai',
        display_name: 'Direct E2E',
        endpoint_id: 'direct-e2e-secondary',
        dialect: 'open_ai_chat',
        base_url: `${iam.url}/v1/`,
        timeout_secs: 30,
        credential_source_id: directCredentialId,
      },
    });
    assert.equal(result.status, 201, JSON.stringify(result.body));
    await rejectedPublication(
      { model_id: directTarget.model_id, provider_id: directTarget.provider_id },
      { type: 'none' },
      /ambiguous; select provider and endpoint/u,
    );
    pass('publication access matrix freezes only compatible, unambiguous direct or brokered access');

    await putDefaultProfile(
      {
        model_id: brokered.model_id,
        provider_id: brokered.provider_id,
        protocol_endpoint_id: brokered.protocol_endpoint_id,
      },
      { type: 'brokered' },
    );

    // Execute the publication, not merely its preview. The authored model carries
    // the preview's exact public binding; publish freezes its brokered provisioning.
    const brokeredAgent = 'brokered-responses-agent';
    result = await req(base, 'PUT', `/v1/config/agents/${brokeredAgent}`, cachedToken, {
      body: {
        id: brokeredAgent,
        name: 'Brokered Responses E2E',
        system: 'Use the managed Cloud model.',
        max_steps: 2,
        model: { mode: 'auto' },
        tools: [{
          type: 'custom',
          name: 'cloud_echo',
          description: 'Echo a value through the managed client-tool boundary.',
          input_schema: {
            type: 'object',
            properties: { value: { type: 'string' } },
            required: ['value'],
          },
        }],
      },
    });
    assert.equal(result.status, 200, JSON.stringify(result.body));
    result = await req(
      base,
      'POST',
      `/v1/config/agents/${brokeredAgent}/publish`,
      cachedToken,
    );
    assert.equal(result.status, 200, JSON.stringify(result.body));
    assert.equal(result.body.installed, true);

    const client = new Anthropic({ apiKey: cachedToken, baseURL: base });
    const betas = ['managed-agents-2026-04-01'];
    const runBrokeredTurn = async (content) => {
      const isolated = await client.beta.sessions.create({
        agent: brokeredAgent,
        environment_id: 'env_local',
        betas,
      });
      await client.beta.sessions.events.send(isolated.id, {
        events: [{ type: 'user.message', content }],
        betas,
      });
      const isolatedEvents = [];
      for await (const event of client.beta.sessions.events.list(isolated.id, { betas })) {
        isolatedEvents.push(event);
      }
      return isolatedEvents;
    };
    const session = await client.beta.sessions.create({
      agent: brokeredAgent,
      environment_id: 'env_local',
      betas,
    });
    await client.beta.sessions.events.send(session.id, {
      events: [{ type: 'user.message', content: [{ type: 'text', text: 'hello cloud' }] }],
      betas,
    });
    let events = [];
    for await (const event of client.beta.sessions.events.list(session.id, { betas })) {
      events.push(event);
    }
    const customUse = events.find((event) => event.type === 'agent.custom_tool_use');
    assert.equal(customUse?.name, 'cloud_echo', JSON.stringify(events));
    await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.custom_tool_result',
        custom_tool_use_id: customUse.id,
        content: [{ type: 'text', text: 'CLOUD-TOOL-E2E' }],
      }],
      betas,
    });
    events = [];
    for await (const event of client.beta.sessions.events.list(session.id, { betas })) {
      events.push(event);
    }
    assert.match(
      JSON.stringify(events),
      /BROKERED-RESPONSES-E2E/u,
      JSON.stringify({ events, gatewayCalls: iam.gatewayCalls, grantCalls: iam.grantCalls }),
    );
    assert.equal(iam.gatewayCalls.length, 2, JSON.stringify(iam.gatewayCalls));
    assert.equal(iam.gatewayCalls[0].body.model, 'gpt-5-e2e');
    assert.equal(iam.gatewayCalls[0].body.store, false);
    assert.equal(iam.gatewayCalls[0].body.tools[0].name, 'cloud_echo');
    assert.ok(
      iam.gatewayCalls[1].body.input.some((item) => (
        item.type === 'function_call'
        && item.call_id === 'call-brokered-custom'
      )),
      JSON.stringify(iam.gatewayCalls[1].body.input),
    );
    assert.ok(
      iam.gatewayCalls[1].body.input.some((item) => (
        item.type === 'function_call_output'
        && item.call_id === 'call-brokered-custom'
      )),
      JSON.stringify(iam.gatewayCalls[1].body.input),
    );
    const createdGrants = iam.grantCalls.filter((call) => call.kind === 'create');
    const closedGrants = iam.grantCalls.filter((call) => call.kind === 'close');
    assert.equal(createdGrants.length, 2, JSON.stringify(iam.grantCalls));
    assert.equal(closedGrants.length, 2, JSON.stringify(iam.grantCalls));
    assert.ok(createdGrants.every((grant) => grant.idempotencyKey?.startsWith('awaken-')));
    assert.ok(createdGrants.every((grant) => grant.authorization === `Bearer ${cachedToken}`));
    assert.deepEqual(
      {
        provider: createdGrants[0].body.provider,
        model: createdGrants[0].body.original_model_id,
        protocol: createdGrants[0].body.native_protocol,
      },
      { provider: 'openai', model: 'gpt-5-e2e', protocol: 'openai_responses' },
    );
    assert.deepEqual(
      iam.gatewayCalls.map((gatewayCall) => gatewayCall.authorization),
      createdGrants.map((grant) => `Bearer capability-${grant.grantId}`),
    );
    assert.deepEqual(
      closedGrants.map((grant) => grant.grantId).sort(),
      createdGrants.map((grant) => grant.grantId).sort(),
    );
    pass('brokered publication rematerializes one scoped grant per Responses attempt around client action');

    // Grant/Gateway cause graph: C1 grant succeeds; C2 error body is a known
    // Cloud code; C3 an unknown code has a discriminating HTTP status; C4 the
    // Gateway returns HTTP success; C5 its native payload is structurally valid.
    //
    // | Rule | C1 | C2 | C3 | C4 | C5 | Expected effect                         |
    // | E1   | N  | auth/account | - | - | - | login-required Session error       |
    // | E2   | N  | subscription/entitlement | - | - | - | unauthorized error     |
    // | E3   | N  | unavailable/balance/quota | - | - | - | typed terminal error  |
    // | E4   | N  | invalid | - | - | - | binding error                          |
    // | E5   | N  | N | 401/403/429 | - | - | status fallback keeps taxonomy      |
    // | E6   | Y  | - | - | N | - | Session error; issued grant still closes    |
    // | E7   | Y  | - | - | Y | N | Session error; issued grant still closes    |
    // | E8   | Y  | - | - | Y | Y | terminal response                           |
    const grantErrorRows = [
      [401, 'authentication_required'],
      [409, 'account_selection_required'],
      [403, 'subscription_required'],
      [403, 'model_not_entitled'],
      [404, 'model_unavailable'],
      [402, 'insufficient_balance'],
      [429, 'quota_exceeded', '7'],
      [400, 'invalid_request'],
      [500, 'temporarily_unavailable'],
      [401, 'unknown_authentication_code'],
      [403, 'unknown_entitlement_code'],
      [429, 'unknown_quota_code', '11'],
    ];
    for (const [status, code, retryAfter] of grantErrorRows) {
      const gatewayCount = iam.gatewayCalls.length;
      const closeCount = iam.grantCalls.filter((entry) => entry.kind === 'close').length;
      const retryable = code === 'quota_exceeded'
        || code === 'temporarily_unavailable'
        || (code === 'unknown_quota_code' && status === 429);
      iam.failGrants(status, code, retryAfter, retryable ? 10 : 1);
      let failedEvents;
      try {
        failedEvents = await runBrokeredTurn([
          { type: 'text', text: `exercise grant rejection ${code}` },
        ]);
      } finally {
        iam.clearGrantFailures();
      }
      assert.ok(
        failedEvents.some((event) => event.type === 'session.error'),
        `${code}: ${JSON.stringify(failedEvents)}`,
      );
      assert.equal(iam.gatewayCalls.length, gatewayCount, `${code} must fail before Gateway I/O`);
      assert.equal(
        iam.grantCalls.filter((entry) => entry.kind === 'close').length,
        closeCount,
        `${code} did not issue a closable grant`,
      );
    }
    pass('Cloud grant rejection matrix preserves the public error taxonomy before Gateway I/O');

    for (const [label, responseBody, status] of [
      ['gateway HTTP error', { error: 'gateway_down' }, 503],
      ['invalid Responses JSON', '{not-json', 200],
      ['invalid function arguments', {
        status: 'completed',
        output: [{ type: 'function_call', call_id: 'bad-call', name: 'cloud_echo', arguments: '{' }],
      }, 200],
    ]) {
      const createCount = iam.grantCalls.filter((entry) => entry.kind === 'create').length;
      const closeCount = iam.grantCalls.filter((entry) => entry.kind === 'close').length;
      iam.respondNextGateway(responseBody, status, 10);
      let failedEvents;
      try {
        failedEvents = await runBrokeredTurn([{ type: 'text', text: label }]);
      } finally {
        iam.clearGatewayResponses();
      }
      assert.ok(
        failedEvents.some((event) => event.type === 'session.error'),
        `${label}: ${JSON.stringify(failedEvents)}`,
      );
      const createsAfter = iam.grantCalls.filter((entry) => entry.kind === 'create').length;
      const closesAfter = iam.grantCalls.filter((entry) => entry.kind === 'close').length;
      assert.equal(closesAfter - closeCount, createsAfter - createCount);
      assert.ok(
        createsAfter > createCount,
        `${label} must exercise at least one issued capability`,
      );
    }
    pass('Gateway HTTP, JSON, and function-argument failures close the issued grant');

    const terminalResponses = [
      {
        status: 'completed',
        output: [{ type: 'future_output_kind', ignored: true }],
        usage: { input_tokens: 1, output_tokens: 0 },
      },
      {
        status: 'incomplete',
        incomplete_details: { reason: 'max_output_tokens' },
        output: [{ type: 'message', content: [{ type: 'output_text', text: 'MAX' }] }],
      },
      {
        status: 'incomplete',
        incomplete_details: { reason: 'content_filter' },
        output: [{ type: 'message', content: [{ type: 'output_text', text: 'FILTERED' }] }],
      },
      {
        status: 'incomplete',
        incomplete_details: { reason: 'future_reason' },
        output: [{ type: 'message', content: [{ type: 'output_text', text: 'FUTURE' }] }],
      },
    ];
    for (const responseBody of terminalResponses) {
      iam.respondNextGateway(responseBody);
      const terminalEvents = await runBrokeredTurn([{ type: 'text', text: 'terminal response matrix' }]);
      assert.ok(
        !terminalEvents.some((event) => event.type === 'session.error'),
        JSON.stringify(terminalEvents),
      );
    }

    iam.respondNextGateway({
      status: 'completed',
      output: [{ type: 'message', content: [{ type: 'output_text', text: 'VISION-OK' }] }],
    });
    const multimodalEvents = await runBrokeredTurn([
      { type: 'text', text: 'inspect both image sources' },
      { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'aW1hZ2U=' } },
      { type: 'image', source: { type: 'url', url: 'https://images.example.test/e2e.png' } },
    ]);
    assert.match(JSON.stringify(multimodalEvents), /VISION-OK/u);
    const multimodalInput = iam.gatewayCalls.at(-1).body.input.find(
      (item) => item.type === 'message' && Array.isArray(item.content),
    );
    assert.deepEqual(
      multimodalInput.content,
      [
        { type: 'input_text', text: 'inspect both image sources' },
        { type: 'input_image', image_url: 'data:image/png;base64,aW1hZ2U=' },
        { type: 'input_image', image_url: 'https://images.example.test/e2e.png' },
      ],
    );
    pass('Responses terminal and multimodal matrix preserves native semantics and forward compatibility');

    const callsBeforeStableFailure = iam.cloudCalls.length;
    iam.failNextReadiness(401, 'authentication_required');
    result = await req(base, 'POST', '/v1/config/brokered-models/refresh');
    assert.equal(result.status, 503, JSON.stringify(result.body));
    assert.match(result.body.detail, /Cloud authentication is required/u);
    assert.equal(
      iam.cloudCalls.slice(callsBeforeStableFailure)
        .filter((call) => call.injected_failure === 'authentication_required').length,
      1,
      'the stable catalog error must reach the product exactly once',
    );
    result = await req(base, 'GET', '/v1/config/catalog');
    assert.deepEqual(
      result.body.offerings.find((offering) => offering.model_id === 'gpt-5-e2e'),
      brokered,
      'a failed refresh must not mutate the last authoritative snapshot',
    );
    pass('stable Cloud catalog errors fail closed without retry or snapshot mutation');

    // Projection validation decision table:
    // | Rule | native protocol | provider/model/revision | attributes | Effect |
    // | P1   | unsupported     | valid                   | valid      | reject |
    // | P2   | supported       | missing/zero/overflow   | valid      | reject |
    // | P3   | supported       | valid                   | duplicate  | minima |
    const canonicalCloudModels = [{
      provider: 'openai',
      original_model_id: 'gpt-5-e2e',
      native_protocol: 'openai_responses',
      context_window: 400000,
      max_output_tokens: 128000,
      capabilities: ['responses'],
      route_publication_revision: 7,
    }];
    const invalidProjectionRows = [
      [503, { ...canonicalCloudModels[0], native_protocol: 'future_responses' }, /unsupported native protocol/u],
      [422, { ...canonicalCloudModels[0], provider: '' }, /provider, model and positive publication revision/u],
      [422, { ...canonicalCloudModels[0], original_model_id: '' }, /provider, model and positive publication revision/u],
      [422, { ...canonicalCloudModels[0], route_publication_revision: 0 }, /positive publication revision/u],
      [422, { ...canonicalCloudModels[0], route_publication_revision: 9223372036854776000 }, /exceeds the local catalog range/u],
    ];
    for (const [expectedStatus, invalidModel, message] of invalidProjectionRows) {
      iam.setCloudModels([invalidModel]);
      const invalidRefresh = await req(base, 'POST', '/v1/config/brokered-models/refresh');
      assert.equal(invalidRefresh.status, expectedStatus, JSON.stringify(invalidRefresh.body));
      assert.match(invalidRefresh.body.detail, message);
      const unchanged = await req(base, 'GET', '/v1/config/catalog');
      assert.deepEqual(
        unchanged.body.offerings.find((offering) => offering.model_id === 'gpt-5-e2e'),
        brokered,
        'an invalid projection must not partially replace the prior snapshot',
      );
    }

    iam.setCloudModels([
      { ...canonicalCloudModels[0], provider: 'openai-a', context_window: 400000, max_output_tokens: null },
      { ...canonicalCloudModels[0], provider: 'openai-b', context_window: 200000, max_output_tokens: 128000 },
      { ...canonicalCloudModels[0], provider: 'openai-c', context_window: null, max_output_tokens: 64000 },
      {
        ...canonicalCloudModels[0],
        provider: 'openai-d', original_model_id: 'attribute-unknown-e2e',
        context_window: null, max_output_tokens: null,
      },
    ]);
    result = await req(base, 'POST', '/v1/config/brokered-models/refresh');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    result = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(result.body.model_attributes['gpt-5-e2e'].context_window, 200000);
    assert.equal(result.body.model_attributes['gpt-5-e2e'].max_output_tokens, 64000);
    assert.equal(result.body.model_attributes['attribute-unknown-e2e'], undefined);
    iam.setCloudModels(canonicalCloudModels);
    result = await req(base, 'POST', '/v1/config/brokered-models/refresh');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    pass('Cloud projection validation is atomic and duplicate attributes merge conservatively');

    iam.setCloudModels([]);
    const callsBeforeTransientFailure = iam.cloudCalls.length;
    iam.failNextReadiness(503, 'temporarily_unavailable');
    result = await req(base, 'POST', '/v1/config/brokered-models/refresh');
    assert.equal(result.status, 200, JSON.stringify(result.body));
    const transientCalls = iam.cloudCalls.slice(callsBeforeTransientFailure);
    assert.equal(
      transientCalls.filter((call) => call.injected_failure === 'temporarily_unavailable').length,
      1,
      'the transient catalog error must reach the product exactly once',
    );
    assert.ok(
      transientCalls.filter((call) => call.path === '/v1/inference/readiness').length >= 2
        && transientCalls.some((call) => call.path === '/v1/inference/models'),
      `the failed readiness must be retried through a complete snapshot: ${JSON.stringify(transientCalls)}`,
    );
    assert.equal(result.body.marked_unavailable, 1);
    result = await req(base, 'GET', '/v1/config/catalog');
    assert.equal(
      result.body.offerings.find((offering) => offering.model_id === 'gpt-5-e2e').status,
      'unavailable',
    );
    assert.equal(result.body.model_attributes?.['gpt-5-e2e'], undefined);
    pass('transient Cloud catalog failure retries, then reconciles removal and Cloud metadata');

    // Cloud identity never exposes the self-managed API-token administration
    // surface, even when the remote PDP would otherwise allow it.
    const callsBeforeTokenAdmin = iam.calls.length;
    result = await req(base, 'GET', '/v1/config/iam/tokens', explicitToken);
    assert.equal(result.status, 404, 'self-managed token administration is not mounted in cloud mode');
    assert.equal(iam.calls.length, callsBeforeTokenAdmin, 'token administration does not reach PDP');

    for (const invalid of ['not-a-jwt', wrongAudienceToken]) {
      assert.equal((await req(base, 'GET', '/v1/files', invalid)).status, 401);
      assert.equal((await req(base, 'GET', '/v1/config/catalog', invalid)).status, 401);
    }
    for (const uri of ['/v1/files', '/v1/config/catalog']) {
      const expired = await req(base, 'GET', uri, expiredToken);
      assert.equal(expired.status, 401);
      assert.match(expired.body.error.message, /cloud access token is expired/u);
    }
    pass('malformed/wrong-audience tokens fail closed and expiry keeps its reason');

    iam.decide('deny');
    assert.equal((await req(base, 'GET', '/v1/files', explicitToken)).status, 403);
    assert.equal((await req(base, 'GET', '/v1/config/catalog', explicitToken)).status, 403);
    iam.decide('require_approval');
    assert.equal((await req(base, 'GET', '/v1/skills', explicitToken)).status, 403);
    assert.equal((await req(base, 'GET', '/v1/config/catalog', explicitToken)).status, 403);
    pass('remote deny and approval obligations remain fail-closed at both PEPs');

    console.log('E2E PASS: Awaken Cloud login and remote authorization stay outside resource services.');
  } finally {
    if (server) await stopServer(server);
    await iam.close();
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
