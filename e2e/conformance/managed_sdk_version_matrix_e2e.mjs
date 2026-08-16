// Compatibility gate for the oldest supported and current Anthropic SDKs.
// Both clients intentionally send the same official Managed beta; the server
// must use tolerant request readers and one additive response shape, not infer
// a package version from User-Agent or x-stainless telemetry.

import assert from 'node:assert/strict';
import Anthropic0105 from '@anthropic-ai/sdk-0-105';
import Anthropic0117 from '@anthropic-ai/sdk-0-117';
import { pass, withRealServer } from '../harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38137);
const BETAS = ['managed-agents-2026-04-01'];
const CLIENTS = [
  ['0.105.0', Anthropic0105],
  ['0.117.1', Anthropic0117],
];

async function exercise(version, Client, baseURL, options) {
  const client = new Client({ apiKey: options.apiKey, baseURL });
  const create = { agent: options.agent, betas: BETAS };
  if (options.environmentId) create.environment_id = options.environmentId;
  const session = await client.beta.sessions.create(create);
  try {
    assert.equal(session.type, 'session', `${version}: create`);

    const retrieved = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(retrieved.id, session.id, `${version}: retrieve`);

    await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `sdk-${version}` }],
      }],
      betas: BETAS,
    });
    const events = [];
    for await (const event of client.beta.sessions.events.list(session.id, { betas: BETAS })) {
      events.push(event);
    }
    assert.ok(events.some((event) => event.type === 'agent.message'), `${version}: event list`);
    assert.ok(events.some((event) => event.type === 'session.status_idle'), `${version}: lifecycle`);
    pass(`Managed SDK ${version} lifecycle`);
  } finally {
    await client.beta.sessions.delete(session.id, { betas: BETAS });
  }
}

async function exerciseVersionSelectionBoundary(baseURL, options) {
  const body = {
    agent: options.agent,
    ...(options.environmentId ? { environment_id: options.environmentId } : {}),
  };
  const request = (userAgent, beta = BETAS[0]) => fetch(`${baseURL}/v1/sessions`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'x-api-key': options.apiKey,
      'anthropic-version': '2023-06-01',
      ...(beta ? { 'anthropic-beta': beta } : {}),
      'user-agent': userAgent,
    },
    body: JSON.stringify(body),
  });
  const oldAgent = await request('anthropic-sdk-typescript/0.105.0');
  const newAgent = await request('anthropic-sdk-typescript/0.117.1');
  assert.equal(oldAgent.status, newAgent.status, 'User-Agent must not select a schema');
  assert.equal(oldAgent.status, 200, 'the supported beta selects the Managed contract');
  const created = [await oldAgent.json(), await newAgent.json()];
  assert.deepEqual(
    Object.keys(created[0]).sort(),
    Object.keys(created[1]).sort(),
    'different client versions receive one additive response schema',
  );

  const absentBeta = await request('anthropic-sdk-typescript/0.117.1', null);
  assert.equal(absentBeta.status, 400, 'the Managed family fails closed without its beta');
  for (const session of created) {
    await fetch(`${baseURL}/v1/sessions/${session.id}`, {
      method: 'DELETE',
      headers: {
        'x-api-key': options.apiKey,
        'anthropic-version': '2023-06-01',
        'anthropic-beta': BETAS[0],
      },
    });
  }
  pass('beta header selects the contract; SDK User-Agent does not select a version');
}

async function main() {
  // Cause/effect graph: the same public ingress and beta receive requests from
  // the oldest supported and current SDKs; both must create, retrieve, run and
  // list a Session without a private version selector.
  // Decision table: supported SDK + official beta => one canonical behavior;
  // absent beta => existing protocol guard rejects; User-Agent differences =>
  // no routing effect.
  const remoteBaseURL = process.env.AWAKEN_MANAGED_BASE_URL;
  const options = remoteBaseURL ? {
    apiKey: process.env.AWAKEN_MANAGED_API_KEY,
    agent: process.env.AWAKEN_MANAGED_AGENT_ID,
    environmentId: process.env.AWAKEN_MANAGED_ENVIRONMENT_ID,
  } : {
    apiKey: 'e2e-dummy',
    agent: 'assistant',
    environmentId: 'env_local',
  };
  assert.ok(options.apiKey, 'AWAKEN_MANAGED_API_KEY is required for a remote matrix');
  assert.ok(options.agent, 'AWAKEN_MANAGED_AGENT_ID is required for a remote matrix');
  const run = async (baseURL) => {
    await exerciseVersionSelectionBoundary(baseURL, options);
    for (const [version, Client] of CLIENTS) await exercise(version, Client, baseURL, options);
  };
  if (remoteBaseURL) await run(remoteBaseURL);
  else await withRealServer('echo', PORT, run);
  console.log('E2E PASS: Managed Agents supports Anthropic SDK 0.105.0 and 0.117.1 on one beta contract.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exit(1);
});
