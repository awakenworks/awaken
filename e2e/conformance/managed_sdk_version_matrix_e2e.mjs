// Compatibility gate for the oldest supported, prior reviewed, and current
// Anthropic SDKs.
// Session calls intentionally send the same official Managed beta. Memory
// calls intentionally omit `betas`: SDK 0.105 injects the legacy Managed beta
// while SDK 0.117 injects the replacement Memory beta. The server must expose
// one additive contract without inferring a package version from telemetry.

import assert from 'node:assert/strict';
import Anthropic0105 from '@anthropic-ai/sdk-0-105';
import Anthropic0117 from '@anthropic-ai/sdk-0-117';
import Anthropic0120 from '@anthropic-ai/sdk-0-120';
import { pass, waitForSessionEventReceipt, withRealServer } from '../harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38137);
const BETAS = ['managed-agents-2026-04-01'];
const MEMORY_BETA = 'agent-memory-2026-07-22';
const CLIENTS = [
  ['0.105.0', Anthropic0105],
  ['0.117.1', Anthropic0117],
  ['0.120.0', Anthropic0120],
];

async function drain(items) {
  const drained = [];
  for await (const item of items) drained.push(item);
  return drained;
}

function resourceMethods(resource, prefix = '', depth = 0, found = []) {
  if (!resource || depth > 3) return found;
  for (const name of Object.getOwnPropertyNames(Object.getPrototypeOf(resource) ?? {})) {
    if (name !== 'constructor' && typeof resource[name] === 'function') found.push(`${prefix}${name}`);
  }
  for (const name of Object.keys(resource)) {
    if (name !== '_client' && resource[name] && typeof resource[name] === 'object') {
      resourceMethods(resource[name], `${prefix}${name}.`, depth + 1, found);
    }
  }
  return found.sort();
}

function assertSdkCapabilityBoundary() {
  const resources = CLIENTS.map(([version, Client]) => [
    version,
    new Client({ apiKey: 'surface-inventory' }).beta, // awaken-allow: secret
  ]);
  const keys = resources.map(([, beta]) => Object.keys(beta).filter((key) => key !== '_client').sort());
  const oldOnly = keys[0].filter((key) => !keys[1].includes(key));
  const currentOnly = keys[1].filter((key) => !keys[0].includes(key));
  assert.deepEqual(oldOnly, [], 'the supported old SDK has no removed Beta resource family');
  assert.deepEqual(
    currentOnly,
    ['dreams', 'tunnels'],
    'Dreams and current Tunnels are explicit current-SDK capability boundaries',
  );
  assert.deepEqual(
    keys[1],
    keys[2],
    '0.120 retains the reviewed 0.117 Beta resource families while adding GA roots',
  );
  const shared = keys[0].filter((key) => keys[1].includes(key));
  for (const key of shared) {
    for (const [, beta] of resources.slice(1)) {
      assert.deepEqual(
        resourceMethods(resources[0][1][key]),
        resourceMethods(beta[key]),
        `${key}: generated method/nested-resource surface differs across supported SDKs`,
      );
    }
  }
  pass(`${shared.length} shared Beta resource families have identical generated method surfaces`);
  pass('Dreams and Tunnels remain tested as explicit current-SDK-only capabilities');
}

async function rawMemoryPage(baseURL, options, storeID, beta, page) {
  const query = new URLSearchParams({ beta: 'true', limit: '1' });
  if (page) query.set('page', page);
  const response = await fetch(
    `${baseURL}/v1/memory_stores/${storeID}/memories?${query}`,
    {
      headers: {
        'x-api-key': options.apiKey,
        'anthropic-version': '2023-06-01',
        'anthropic-beta': beta,
      },
    },
  );
  const body = await response.json();
  assert.equal(response.status, 200, `raw Memory page using ${beta}: ${JSON.stringify(body)}`);
  return body;
}

async function exercise(version, Client, baseURL, options) {
  const client = new Client({ apiKey: options.apiKey, baseURL });
  const create = { agent: options.agent, betas: BETAS };
  if (options.environmentId) create.environment_id = options.environmentId;
  const session = await client.beta.sessions.create(create);
  try {
    assert.equal(session.type, 'session', `${version}: create`);

    const retrieved = await client.beta.sessions.retrieve(session.id, { betas: BETAS });
    assert.equal(retrieved.id, session.id, `${version}: retrieve`);

    const receipt = await client.beta.sessions.events.send(session.id, {
      events: [{
        type: 'user.message',
        content: [{ type: 'text', text: `sdk-${version}` }],
      }],
      betas: BETAS,
    });
    const acceptedId = receipt.data?.[0]?.id;
    assert.equal(typeof acceptedId, 'string', `${version}: exact accepted User Event id`);
    // SDK lifecycle rule: C1=the versioned client returns an exact receipt and
    // C2=message+idle follow it; E=that SDK observes one canonical lifecycle.
    // K=older history is excluded. R=C1+C2=>E; otherwise retry/fail boundedly.
    const { delta: events } = await waitForSessionEventReceipt(
      client,
      session.id,
      acceptedId,
      BETAS,
      ({ delta }) => delta.some((event) => event.type === 'agent.message')
        && delta.some((event) => event.type === 'session.status_idle'),
      `${version}: accepted receipt to settle through the canonical lifecycle`,
    );
    assert.ok(events.some((event) => event.type === 'agent.message'), `${version}: event list`);
    assert.ok(events.some((event) => event.type === 'session.status_idle'), `${version}: lifecycle`);
    pass(`Managed SDK ${version} lifecycle`);
  } finally {
    await client.beta.sessions.delete(session.id, { betas: BETAS });
  }
}

async function exerciseMemory(version, Client, baseURL, options) {
  const client = new Client({ apiKey: options.apiKey, baseURL });
  let store;
  const memoryIDs = new Set();
  try {
    // Do not pass `betas` anywhere in this function. This verifies each
    // generated SDK's endpoint-specific default header, not a test override.
    store = await client.beta.memoryStores.create({
      name: `sdk-${version}-${Date.now()}`,
      description: 'cross-version compatibility fixture',
      metadata: { sdk: version },
    });
    assert.equal(store.type, 'memory_store', `${version}: MemoryStore create`);

    const retrievedStore = await client.beta.memoryStores.retrieve(store.id);
    assert.equal(retrievedStore.id, store.id, `${version}: MemoryStore retrieve`);
    const updatedStore = await client.beta.memoryStores.update(store.id, {
      description: 'updated compatibility fixture',
    });
    assert.equal(updatedStore.description, 'updated compatibility fixture');
    const stores = await drain(client.beta.memoryStores.list());
    assert.ok(stores.some((item) => item.id === store.id), `${version}: MemoryStore list`);

    const first = await client.beta.memoryStores.memories.create(store.id, {
      path: '/current.md',
      content: 'first',
      view: 'full',
    });
    memoryIDs.add(first.id);
    assert.equal(first.content, 'first', `${version}: Memory create full projection`);

    await assert.rejects(
      () => client.beta.memoryStores.memories.update(first.id, {
        memory_store_id: store.id,
        content: 'must-not-commit',
        precondition: {
          type: 'content_sha256',
          content_sha256: 'deadbeef'.repeat(8),
        },
      }),
      (error) => error?.status === 409,
      `${version}: stale Memory precondition must fail`,
    );
    const afterStale = await client.beta.memoryStores.memories.retrieve(first.id, {
      memory_store_id: store.id,
    });
    assert.equal(afterStale.content, 'first', `${version}: rejected update is non-mutating`);

    const updated = await client.beta.memoryStores.memories.update(first.id, {
      memory_store_id: store.id,
      content: 'second',
      view: 'full',
      precondition: {
        type: 'content_sha256',
        content_sha256: first.content_sha256,
      },
    });
    assert.equal(updated.content, 'second', `${version}: fresh Memory update`);
    assert.notEqual(updated.memory_version_id, first.memory_version_id);

    const second = await client.beta.memoryStores.memories.create(store.id, {
      path: '/archive/old.md',
      content: 'archived',
    });
    memoryIDs.add(second.id);
    const prefix = await drain(client.beta.memoryStores.memories.list(store.id, {
      path_prefix: '/archive/',
    }));
    assert.deepEqual(prefix.map((item) => item.id), [second.id], `${version}: path_prefix`);

    // A cursor minted under either supported beta must resume under the other:
    // the selectors choose one canonical repository and pagination contract.
    const legacyFirst = await rawMemoryPage(baseURL, options, store.id, BETAS[0]);
    assert.equal(legacyFirst.data.length, 1, `${version}: legacy first page`);
    assert.ok(legacyFirst.next_page, `${version}: legacy cursor exists`);
    const currentResume = await rawMemoryPage(
      baseURL,
      options,
      store.id,
      MEMORY_BETA,
      legacyFirst.next_page,
    );
    assert.equal(currentResume.data.length, 1, `${version}: current beta resumes legacy cursor`);
    assert.notEqual(currentResume.data[0].id, legacyFirst.data[0].id);

    const currentFirst = await rawMemoryPage(baseURL, options, store.id, MEMORY_BETA);
    assert.ok(currentFirst.next_page, `${version}: current cursor exists`);
    const legacyResume = await rawMemoryPage(
      baseURL,
      options,
      store.id,
      BETAS[0],
      currentFirst.next_page,
    );
    assert.equal(legacyResume.data.length, 1, `${version}: legacy beta resumes current cursor`);
    assert.notEqual(legacyResume.data[0].id, currentFirst.data[0].id);

    const versions = await drain(client.beta.memoryStores.memoryVersions.list(store.id));
    assert.ok(versions.some((item) => item.operation === 'created'), `${version}: created version`);
    assert.ok(versions.some((item) => item.operation === 'modified'), `${version}: modified version`);
    const versionItem = versions[0];
    const retrievedVersion = await client.beta.memoryStores.memoryVersions.retrieve(versionItem.id, {
      memory_store_id: store.id,
    });
    assert.equal(retrievedVersion.id, versionItem.id, `${version}: version retrieve`);
    const redacted = await client.beta.memoryStores.memoryVersions.redact(versionItem.id, {
      memory_store_id: store.id,
    });
    assert.ok(redacted.redacted_at, `${version}: version redact`);

    for (const memoryID of memoryIDs) {
      const deleted = await client.beta.memoryStores.memories.delete(memoryID, {
        memory_store_id: store.id,
      });
      assert.equal(deleted.type, 'memory_deleted', `${version}: Memory delete`);
    }
    memoryIDs.clear();
    const archived = await client.beta.memoryStores.archive(store.id);
    assert.ok(archived.archived_at, `${version}: MemoryStore archive`);
    const deletedStore = await client.beta.memoryStores.delete(store.id);
    assert.equal(deletedStore.type, 'memory_store_deleted', `${version}: MemoryStore delete`);
    store = undefined;

    pass(`Memory SDK ${version} default beta, CRUD, CAS, cursor, versions and cleanup`);
    return {
      store: Object.keys(retrievedStore).sort(),
      memory: Object.keys(first).sort(),
      version: Object.keys(retrievedVersion).sort(),
    };
  } finally {
    if (store) {
      for (const memoryID of memoryIDs) {
        try {
          await client.beta.memoryStores.memories.delete(memoryID, {
            memory_store_id: store.id,
          });
        } catch {}
      }
      try { await client.beta.memoryStores.archive(store.id); } catch {}
      try { await client.beta.memoryStores.delete(store.id); } catch {}
    }
  }
}

async function exerciseVersionSelectionBoundary(baseURL, options) {
  const body = {
    agent: options.agent,
    ...(options.environmentId ? { environment_id: options.environmentId } : {}),
  };
  const request = (userAgent, beta = BETAS[0], version = '2023-06-01') => fetch(`${baseURL}/v1/sessions`, {
    method: 'POST',
    headers: {
      'content-type': 'application/json',
      'x-api-key': options.apiKey,
      ...(version ? { 'anthropic-version': version } : {}),
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
  const unknownBeta = await request('anthropic-sdk-typescript/0.117.1', 'future-managed-beta');
  assert.equal(unknownBeta.status, 400, 'an unknown beta alone cannot select the contract');
  const mixedBeta = await request(
    'anthropic-sdk-typescript/0.117.1',
    `future-managed-beta, ${BETAS[0]}`,
  );
  assert.equal(mixedBeta.status, 200, 'an additive unknown beta does not hide the supported beta');
  created.push(await mixedBeta.json());
  const duplicateBeta = await request(
    'anthropic-sdk-typescript/0.105.0',
    `${BETAS[0]}, ${BETAS[0]}`,
  );
  assert.equal(duplicateBeta.status, 200, 'a duplicated supported beta is idempotent');
  created.push(await duplicateBeta.json());
  const wrongVersion = await request(
    'anthropic-sdk-typescript/0.117.1',
    BETAS[0],
    '2099-01-01',
  );
  assert.equal(wrongVersion.status, 400, 'an explicit unsupported API version fails closed');
  const legacyMissingVersion = await request(
    'awaken-legacy-raw-client/1',
    BETAS[0],
    null,
  );
  assert.equal(legacyMissingVersion.status, 200, 'legacy raw clients may omit anthropic-version');
  created.push(await legacyMissingVersion.json());
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
  // the oldest supported and current SDKs; both must create, retrieve, return
  // an exact durable receipt, settle its Run, and list a Session without a
  // private version selector. Effects include the same receipt id becoming
  // processed before its message/idle projection; deletion cannot race an
  // admitted Run. Decision table: supported SDK + official beta => one
  // canonical async lifecycle; accepted but unsettled => keep polling; absent
  // beta => existing protocol guard rejects; User-Agent differences => no
  // routing effect. Constraints/invariant: beta/API headers select the contract,
  // never User-Agent, and both SDK versions use the same server repositories.
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
  assertSdkCapabilityBoundary();
  const run = async (baseURL) => {
    await exerciseVersionSelectionBoundary(baseURL, options);
    for (const [version, Client] of CLIENTS) await exercise(version, Client, baseURL, options);
    const memoryShapes = [];
    for (const [version, Client] of CLIENTS) {
      memoryShapes.push(await exerciseMemory(version, Client, baseURL, options));
    }
    for (const shape of memoryShapes.slice(1)) {
      assert.deepEqual(
        memoryShapes[0],
        shape,
        'legacy and current Memory SDKs receive one additive response schema',
      );
    }
    pass('Memory SDK 0.105.0, 0.117.1 and 0.120.0 share response and cursor contracts');
  };
  if (remoteBaseURL) await run(remoteBaseURL);
  else await withRealServer('echo', PORT, run);
  console.log('E2E PASS: Managed Agents supports Anthropic SDK 0.105.0, 0.117.1 and 0.120.0 across Managed and Memory beta selectors.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exit(1);
});
