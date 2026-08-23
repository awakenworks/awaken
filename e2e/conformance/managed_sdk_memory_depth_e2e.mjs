// Memory compatibility depth: bidirectional SDK handoff, mixed-version CAS,
// every mutating subresource's beta rejection, cross-beta cursors, cursor
// invalidation safety, and literal HTTP/1 header framing.
//
// Cause/effect graph: SDK 0.105/0.117/0.120 or header selector -> one Memory aggregate/cursor ->
// mutation or read. Valid old/current selectors share state; ambiguous/invalid
// selectors fail before mutation; stale cursors never resurrect removed heads.
// Decision table coverage is grouped in the assertions below by handoff, CAS,
// mutation admission, pagination and raw-header framing.

import assert from 'node:assert/strict';
import net from 'node:net';
import Anthropic0105 from '@anthropic-ai/sdk-0-105';
import Anthropic0117 from '@anthropic-ai/sdk-0-117';
import Anthropic0120 from '@anthropic-ai/sdk-0-120';
import { pass, withScenarioServer } from '../harness.mjs';

const LEGACY_BETA = 'managed-agents-2026-04-01';
const MEMORY_BETA = 'agent-memory-2026-07-22';
const CLIENTS = [
  ['0.105.0', Anthropic0105],
  ['0.117.1', Anthropic0117],
  ['0.120.0', Anthropic0120],
];

async function drain(items) {
  const values = [];
  for await (const item of items) values.push(item);
  return values;
}

function headers(apiKey, beta, content = false) {
  return {
    'x-api-key': apiKey,
    'anthropic-version': '2023-06-01',
    ...(beta ? { 'anthropic-beta': beta } : {}),
    ...(content ? { 'content-type': 'application/json' } : {}),
  };
}

async function rawJSON(baseURL, apiKey, method, path, beta, body) {
  const response = await fetch(`${baseURL}${path}`, {
    method,
    headers: headers(apiKey, beta, body !== undefined),
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { response, body: text ? JSON.parse(text) : null };
}

async function rawPage(baseURL, apiKey, path, beta, page) {
  const separator = path.includes('?') ? '&' : '?';
  const url = new URL(`${baseURL}${path}${separator}limit=1${page ? `&page=${encodeURIComponent(page)}` : ''}`);
  const response = await fetch(url, { headers: headers(apiKey, beta) });
  const body = await response.json();
  assert.equal(response.status, 200, `${path} with ${beta}: ${JSON.stringify(body)}`);
  return body;
}

async function exerciseHandoff(baseURL, apiKey, creatorSpec, operatorSpec) {
  const [creatorVersion, Creator] = creatorSpec;
  const [operatorVersion, Operator] = operatorSpec;
  const creator = new Creator({ apiKey, baseURL });
  const operator = new Operator({ apiKey, baseURL });
  const store = await creator.beta.memoryStores.create({
    name: `handoff-${creatorVersion}-to-${operatorVersion}`,
  });
  const memory = await creator.beta.memoryStores.memories.create(store.id, {
    path: '/handoff.md',
    content: creatorVersion,
    view: 'full',
  });
  assert.equal((await operator.beta.memoryStores.retrieve(store.id)).id, store.id);
  assert.equal((await operator.beta.memoryStores.memories.retrieve(memory.id, {
    memory_store_id: store.id,
  })).content, creatorVersion);
  const updated = await operator.beta.memoryStores.memories.update(memory.id, {
    memory_store_id: store.id,
    content: operatorVersion,
    view: 'full',
    precondition: { type: 'content_sha256', content_sha256: memory.content_sha256 },
  });
  assert.equal(updated.content, operatorVersion);
  assert.equal((await creator.beta.memoryStores.memories.delete(memory.id, {
    memory_store_id: store.id,
  })).type, 'memory_deleted');
  assert.ok((await operator.beta.memoryStores.archive(store.id)).archived_at);
  assert.equal((await creator.beta.memoryStores.delete(store.id)).type, 'memory_store_deleted');
  pass(`Memory aggregate handoff ${creatorVersion} -> ${operatorVersion}`);
}

async function exerciseConcurrentCAS(baseURL, apiKey) {
  const oldClient = new Anthropic0105({ apiKey, baseURL });
  const currentClient = new Anthropic0117({ apiKey, baseURL });
  const store = await oldClient.beta.memoryStores.create({ name: 'mixed-version-cas' });
  const memory = await oldClient.beta.memoryStores.memories.create(store.id, {
    path: '/cas.md', content: 'base', view: 'full',
  });
  const before = await drain(oldClient.beta.memoryStores.memoryVersions.list(store.id));
  const operation = (client, content) => client.beta.memoryStores.memories.update(memory.id, {
    memory_store_id: store.id,
    content,
    view: 'full',
    precondition: { type: 'content_sha256', content_sha256: memory.content_sha256 },
  });
  const settled = await Promise.allSettled([
    operation(oldClient, 'old-won'),
    operation(currentClient, 'current-won'),
  ]);
  assert.equal(settled.filter((result) => result.status === 'fulfilled').length, 1);
  const rejected = settled.find((result) => result.status === 'rejected');
  assert.equal(rejected.reason?.status, 409, 'the losing SDK observes CAS conflict');
  const finalMemory = await currentClient.beta.memoryStores.memories.retrieve(memory.id, {
    memory_store_id: store.id,
  });
  assert.ok(['old-won', 'current-won'].includes(finalMemory.content));
  const after = await drain(currentClient.beta.memoryStores.memoryVersions.list(store.id));
  assert.equal(after.length, before.length + 1, 'exactly one version commits');
  await currentClient.beta.memoryStores.memories.delete(memory.id, { memory_store_id: store.id });
  await currentClient.beta.memoryStores.archive(store.id);
  await currentClient.beta.memoryStores.delete(store.id);
  pass('mixed SDK CAS has one winner, one 409, and no lost update');
}

async function exerciseMutationAdmissionAndCursors(baseURL, apiKey) {
  const client = new Anthropic0117({ apiKey, baseURL });
  const store = await client.beta.memoryStores.create({ name: 'negative-subresources' });
  const memory = await client.beta.memoryStores.memories.create(store.id, {
    path: '/protected.md', content: 'original', view: 'full',
  });
  await client.beta.memoryStores.memories.update(memory.id, {
    memory_store_id: store.id,
    content: 'protected',
    precondition: { type: 'content_sha256', content_sha256: memory.content_sha256 },
  });
  const baselineVersions = await drain(client.beta.memoryStores.memoryVersions.list(store.id));
  const version = baselineVersions[0];
  const invalidBetas = [null, 'future-memory-beta', `${LEGACY_BETA},${MEMORY_BETA}`];
  const mutations = [
    ['memory update', 'POST', `/v1/memory_stores/${store.id}/memories/${memory.id}?beta=true`, { content: 'forbidden' }],
    ['memory delete', 'DELETE', `/v1/memory_stores/${store.id}/memories/${memory.id}?beta=true`, undefined],
    ['store archive', 'POST', `/v1/memory_stores/${store.id}/archive?beta=true`, undefined],
    ['version redact', 'POST', `/v1/memory_stores/${store.id}/memory_versions/${version.id}/redact?beta=true`, undefined],
  ];
  for (const beta of invalidBetas) {
    for (const [name, method, path, body] of mutations) {
      const result = await rawJSON(baseURL, apiKey, method, path, beta, body);
      assert.equal(result.response.status, 400, `${name} must reject ${beta ?? 'missing beta'}`);
    }
  }
  const unchangedStore = await client.beta.memoryStores.retrieve(store.id);
  const unchangedMemory = await client.beta.memoryStores.memories.retrieve(memory.id, {
    memory_store_id: store.id,
  });
  const unchangedVersions = await drain(client.beta.memoryStores.memoryVersions.list(store.id));
  assert.equal(unchangedStore.archived_at, null);
  assert.equal(unchangedMemory.content, 'protected');
  assert.deepEqual(
    unchangedVersions.map((item) => [item.id, item.redacted_at]),
    baselineVersions.map((item) => [item.id, item.redacted_at]),
    'all rejected subresource mutations are atomic',
  );

  await client.beta.memoryStores.create({ name: 'store-cursor-peer' });
  const legacyStorePage = await rawPage(baseURL, apiKey, '/v1/memory_stores?beta=true', LEGACY_BETA);
  assert.ok(legacyStorePage.next_page, 'MemoryStore cursor exists');
  const currentStoreResume = await rawPage(
    baseURL, apiKey, '/v1/memory_stores?beta=true', MEMORY_BETA, legacyStorePage.next_page,
  );
  assert.ok(Array.isArray(currentStoreResume.data));
  const currentStorePage = await rawPage(baseURL, apiKey, '/v1/memory_stores?beta=true', MEMORY_BETA);
  const legacyStoreResume = await rawPage(
    baseURL, apiKey, '/v1/memory_stores?beta=true', LEGACY_BETA, currentStorePage.next_page,
  );
  assert.ok(Array.isArray(legacyStoreResume.data));

  const versionPath = `/v1/memory_stores/${store.id}/memory_versions?beta=true`;
  const legacyVersionPage = await rawPage(baseURL, apiKey, versionPath, LEGACY_BETA);
  assert.ok(legacyVersionPage.next_page, 'MemoryVersion cursor exists');
  const currentVersionResume = await rawPage(
    baseURL, apiKey, versionPath, MEMORY_BETA, legacyVersionPage.next_page,
  );
  assert.equal(currentVersionResume.data.length, 1);
  const currentVersionPage = await rawPage(baseURL, apiKey, versionPath, MEMORY_BETA);
  const legacyVersionResume = await rawPage(
    baseURL, apiKey, versionPath, LEGACY_BETA, currentVersionPage.next_page,
  );
  assert.equal(legacyVersionResume.data.length, 1);

  const cursorStore = await client.beta.memoryStores.create({ name: 'cursor-deletion' });
  for (const [path, content] of [['/a.md', 'a'], ['/b.md', 'b'], ['/c.md', 'c']]) {
    await client.beta.memoryStores.memories.create(cursorStore.id, { path, content });
  }
  const allHeads = await drain(client.beta.memoryStores.memories.list(cursorStore.id));
  const memoryPath = `/v1/memory_stores/${cursorStore.id}/memories?beta=true`;
  const stalePage = await rawPage(baseURL, apiKey, memoryPath, LEGACY_BETA);
  assert.equal(stalePage.data[0].id, allHeads[0].id);
  await client.beta.memoryStores.memories.delete(allHeads[1].id, { memory_store_id: cursorStore.id });
  const afterDelete = await rawPage(baseURL, apiKey, memoryPath, MEMORY_BETA, stalePage.next_page);
  assert.ok(!afterDelete.data.some((item) => item.id === allHeads[1].id), 'cursor cannot resurrect deleted head');

  const allStores = await drain(client.beta.memoryStores.list());
  const storePage = await rawPage(baseURL, apiKey, '/v1/memory_stores?beta=true', MEMORY_BETA);
  const afterFirst = allStores.find((item) => item.id !== storePage.data[0].id && !item.archived_at);
  assert.ok(afterFirst, 'an active store exists after the first cursor item');
  await client.beta.memoryStores.archive(afterFirst.id);
  const afterArchive = await rawPage(
    baseURL, apiKey, '/v1/memory_stores?beta=true', LEGACY_BETA, storePage.next_page,
  );
  assert.ok(!afterArchive.data.some((item) => item.id === afterFirst.id), 'cursor cannot resurrect archived store');
  pass('MemoryStore/MemoryVersion cursors cross beta and stale cursors hide removed resources');
}

function rawHTTP(
  baseURL,
  path,
  headerLines,
  { method = 'GET', headerBytes = Buffer.alloc(0), body = Buffer.alloc(0) } = {},
) {
  const url = new URL(baseURL);
  return new Promise((resolve, reject) => {
    const socket = net.createConnection({ host: url.hostname, port: Number(url.port) });
    const chunks = [];
    socket.on('connect', () => {
      const prefix = Buffer.from([
        `${method} ${path} HTTP/1.1`,
        `Host: ${url.host}`,
        'Connection: close',
        ...headerLines,
      ].join('\r\n'));
      socket.write(Buffer.concat([prefix, headerBytes, Buffer.from('\r\n\r\n'), body]));
    });
    socket.on('data', (chunk) => chunks.push(chunk));
    socket.on('end', () => {
      const response = Buffer.concat(chunks).toString('latin1');
      const match = response.match(/^HTTP\/1\.1 (\d{3})/);
      resolve(match ? Number(match[1]) : 0);
    });
    socket.on('error', reject);
  });
}

async function exerciseRawHeaders(baseURL) {
  const key = 'x-api-key: e2e-dummy';
  const direct = '/v1/memory_stores?beta=true';
  const scoped = '/v1/workspaces/default/memory_stores?beta=true';
  for (const path of [direct, scoped]) {
    assert.equal(await rawHTTP(baseURL, path, [key]), 400, `${path}: missing beta`);
    assert.equal(await rawHTTP(baseURL, path, [key, `anthropic-beta: ${MEMORY_BETA}`]), 200);
    assert.equal(await rawHTTP(baseURL, path, [key, `anthropic-beta: ${LEGACY_BETA}`]), 200);
  }
  assert.equal(await rawHTTP(baseURL, direct, [
    key,
    `anthropic-beta: ${MEMORY_BETA}`,
    `anthropic-beta: ${MEMORY_BETA}`,
  ]), 200, 'repeated identical header lines are idempotent');
  for (const lines of [
    [`anthropic-beta: ${LEGACY_BETA}`, `anthropic-beta: ${MEMORY_BETA}`],
    [`anthropic-beta: ${MEMORY_BETA}`, `anthropic-beta: ${LEGACY_BETA}`],
  ]) {
    assert.equal(await rawHTTP(baseURL, direct, [key, ...lines]), 400, 'header order cannot hide ambiguity');
  }
  assert.equal(await rawHTTP(baseURL, direct, [
    key,
    `anthropic-beta:   ${MEMORY_BETA} , ${MEMORY_BETA}  `,
  ]), 200, 'OWS and comma-separated duplicates normalize');
  const mutationBody = Buffer.from(JSON.stringify({ name: 'must-not-exist' }));
  const mutationHeaders = [
    key,
    'anthropic-version: 2023-06-01',
    'content-type: application/json',
    `content-length: ${mutationBody.length}`,
  ];
  const beforeMalformed = await rawJSON(baseURL, 'e2e-dummy', 'GET', direct, MEMORY_BETA);
  const invalidUTF8 = await rawHTTP(baseURL, direct, [
    ...mutationHeaders,
    'anthropic-beta: ',
  ], {
    method: 'POST',
    headerBytes: Buffer.from([0xff]),
    body: mutationBody,
  });
  assert.ok([400, 431].includes(invalidUTF8), `invalid header bytes: ${invalidUTF8}`);
  const oversized = await rawHTTP(baseURL, direct, [
    ...mutationHeaders,
    `anthropic-beta: ${'x'.repeat(64 * 1024)}`,
  ], { method: 'POST', body: mutationBody });
  assert.ok([400, 431].includes(oversized), `oversized beta header: ${oversized}`);
  const afterMalformed = await rawJSON(baseURL, 'e2e-dummy', 'GET', direct, MEMORY_BETA);
  assert.deepEqual(
    afterMalformed.body.data.map((item) => item.id),
    beforeMalformed.body.data.map((item) => item.id),
    'malformed write headers cannot reach resource mutation',
  );
  pass('literal HTTP headers preserve repeats/order/OWS and reject malformed or oversized values');
}

await withScenarioServer('management', 'mcp', 38189, async (baseURL) => {
  const apiKey = 'e2e-dummy';
  await exerciseHandoff(baseURL, apiKey, CLIENTS[0], CLIENTS[1]);
  await exerciseHandoff(baseURL, apiKey, CLIENTS[1], CLIENTS[0]);
  await exerciseHandoff(baseURL, apiKey, CLIENTS[1], CLIENTS[2]);
  await exerciseHandoff(baseURL, apiKey, CLIENTS[2], CLIENTS[1]);
  await exerciseConcurrentCAS(baseURL, apiKey);
  await exerciseMutationAdmissionAndCursors(baseURL, apiKey);
  await exerciseRawHeaders(baseURL);
});

console.log('E2E PASS: Memory cross-version, CAS, negative, cursor and raw-header compatibility depth.');
