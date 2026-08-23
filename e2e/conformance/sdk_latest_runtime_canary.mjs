// Runtime half of the registry-latest SDK canary. The declaration fingerprint
// detects shape drift; this executable smoke detects generated path, default
// beta, pagination and response-decoding drift against the real Awaken
// topologies that own each resource family.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import {
  pass,
  waitForSessionEventReceipt,
  withRealServer,
  withScenarioServer,
} from '../harness.mjs';

const packageRoot = process.env.ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT;
assert.ok(packageRoot, 'ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT is required');
const manifest = JSON.parse(readFileSync(resolve(packageRoot, 'package.json'), 'utf8'));
const { default: Anthropic } = await import(pathToFileURL(resolve(packageRoot, 'index.mjs')));

await withRealServer('echo', 38190, async (baseURL) => {
  // Cause/effect graph: C0=Session Event send returns one exact durable
  // receipt; C1=the registry-latest generated Session and Memory
  // methods use their default beta/header behavior; C2=the echo topology owns
  // those resources. Effects are a decoded terminal Session stream plus Memory
  // CRUD round-trip. Decision rule R1: C0 && C1 && C2 => both resource families work;
  // any generated path/header/decoder drift fails at its official SDK call.
  // Constraints/invariant: the generated SDK methods and existing echo-owned
  // repositories are the only request/response paths; polling observes C0 and
  // cannot add a route, beta override, or second completion authority.
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
  const session = await client.beta.sessions.create({
    agent: 'assistant',
    environment_id: 'env_local',
  });
  try {
    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `latest-sdk-${manifest.version}` }],
      }],
    });
    const acceptedId = receipt.data[0]?.id;
    assert.equal(typeof acceptedId, 'string', 'R1 exact accepted User Event id');
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      acceptedId,
      undefined,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      `registry SDK ${manifest.version} Session Run to settle`,
      { listParams: { limit: 1 } },
    );
    assert.ok(events.some((event) => event.type === 'agent.message'));
    assert.ok(events.some((event) => event.type === 'session.status_idle'));
  } finally {
    await client.beta.sessions.delete(session.id);
  }

  const store = await client.beta.memoryStores.create({ name: `latest-${manifest.version}` });
  const memory = await client.beta.memoryStores.memories.create(store.id, {
    path: '/latest.md',
    content: manifest.version,
    view: 'full',
  });
  assert.equal((await client.beta.memoryStores.memories.retrieve(memory.id, {
    memory_store_id: store.id,
  })).content, manifest.version);
  await client.beta.memoryStores.memories.delete(memory.id, { memory_store_id: store.id });
  await client.beta.memoryStores.archive(store.id);
  await client.beta.memoryStores.delete(store.id);

  pass(`registry SDK ${manifest.version} runs Session and Memory defaults`);
});

// UserProfiles is Control-owned and intentionally absent from the echo runtime
// topology above. Exercise its existing management behavior owner without
// manufacturing a second route in that topology. Do not pass `betas`: the
// generated SDK method's own version header is the runtime behavior under review
// (0.117.1 emits the legacy selector; 0.120.0 emits the access_type selector).
await withScenarioServer('management', 'mcp', 38191, async (baseURL) => {
  // Cause/effect graph: C3=UserProfiles remains Control-owned and absent from
  // echo; C4=the latest SDK supplies its own default version selector. Effect is
  // a create/retrieve round-trip decoded with access_type + relationship.
  // Decision rule R2: C3 && C4 => use the existing management topology without
  // explicit betas; route/header/shape drift fails rather than adding a canary-only route.
  // Constraints/invariant: Control remains the sole UserProfile owner and the
  // generated SDK remains the sole version-header owner; this canary may not
  // mirror either contract in the echo topology or in hand-authored transport.
  const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL });
  const profile = await client.beta.userProfiles.create({
    access_type: 'application',
    external_id: `latest-sdk-${manifest.version}`,
  });
  assert.equal(profile.access_type, 'application');
  assert.equal(profile.relationship, 'external');
  assert.equal((await client.beta.userProfiles.retrieve(profile.id)).id, profile.id);
  pass(`registry SDK ${manifest.version} runs UserProfile default beta`);
});

console.log(`SDK LATEST RUNTIME CANARY PASS: @anthropic-ai/sdk ${manifest.version}.`);
