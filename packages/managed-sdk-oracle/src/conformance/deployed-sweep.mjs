import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const defaultCoveragePath = path.resolve(
  packageRoot,
  '../../contracts/anthropic-managed/operation-coverage.generated.json',
);

function concretePath(template, operationID) {
  const suffix = encodeURIComponent(`qualification-missing-${operationID.replaceAll('.', '-')}`);
  return template.replaceAll('{}', suffix);
}

function isCollectionRead(operation) {
  return operation.method === 'GET' && !operation.path.includes('{}');
}

export function buildDeployedProbePlan(coverage) {
  assert.equal(coverage.schema_version, 2, 'unsupported operation coverage schema');
  const seen = new Set();
  return coverage.operations.map((operation) => {
    assert.ok(!seen.has(operation.id), `duplicate deployed probe ${operation.id}`);
    seen.add(operation.id);
    const documentedTunnel = operation.id.startsWith('documented.organizationTunnels.');
    assert.ok(operation.betas.length <= 1, `${operation.id}: ambiguous public beta policy`);
    const beta = operation.betas[0];
    assert.ok(
      beta || operation.resource === 'models' || !operation.id.startsWith('beta.'),
      `${operation.id}: no public beta policy`,
    );
    return {
      ...operation,
      path: `${concretePath(operation.path, operation.id)}${
        operation.transport_query ? `?${operation.transport_query}` : ''
      }`,
      beta,
      auth: operation.resource === 'tunnels' && !documentedTunnel ? 'bearer' : 'api_key',
      expectedClass: isCollectionRead(operation) ? 'collection' : 'error',
    };
  });
}

function responseShape(body) {
  if (body === null || typeof body !== 'object' || Array.isArray(body)) return typeof body;
  const shape = { keys: Object.keys(body).sort() };
  if (Array.isArray(body.data)) shape.data = 'array';
  if ('has_more' in body) shape.has_more = typeof body.has_more;
  if ('next_page' in body) shape.next_page = body.next_page === null ? 'null' : typeof body.next_page;
  if (body.error && typeof body.error === 'object') {
    shape.error = {
      keys: Object.keys(body.error).sort(),
      type: body.error.type,
      message: body.error.message,
    };
  }
  return shape;
}

function assertCanonicalResponse(probe, status, body, targetName) {
  assert.ok(status < 500, `${targetName}/${probe.id}: public operation returned ${status}`);
  if (probe.expectedClass === 'collection') {
    assert.equal(status, 200, `${targetName}/${probe.id}: collection read`);
    assert.ok(Array.isArray(body?.data), `${targetName}/${probe.id}: collection data`);
    return;
  }
  assert.ok(status >= 400 && status < 500, `${targetName}/${probe.id}: negative partition`);
  assert.equal(body?.type, 'error', `${targetName}/${probe.id}: error envelope type`);
  assert.equal(typeof body?.error?.type, 'string', `${targetName}/${probe.id}: error kind`);
  assert.equal(typeof body?.error?.message, 'string', `${targetName}/${probe.id}: error message`);
}

async function executeProbe(probe, target, fetchImpl) {
  const headers = {
    accept: 'application/json',
    'anthropic-version': '2023-06-01',
  };
  if (probe.beta) headers['anthropic-beta'] = probe.beta;
  if (probe.auth === 'bearer') headers.authorization = `Bearer ${target.tunnelAccessToken}`;
  else headers['x-api-key'] = target.apiKey;
  const request = { method: probe.method, headers, signal: AbortSignal.timeout(15_000) };
  if (!['GET', 'HEAD', 'DELETE'].includes(probe.method)) {
    headers['content-type'] = 'application/json';
    request.body = '{}';
  }
  const response = await fetchImpl(new URL(probe.path, target.baseURL), request);
  const text = await response.text();
  let body = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch (error) {
      assert.fail(`${target.name}/${probe.id}: non-JSON response: ${error.message}`);
    }
  }
  assertCanonicalResponse(probe, response.status, body, target.name);
  return { id: probe.id, status: response.status, shape: responseShape(body) };
}

export function compareDeployedResults(actual, reference) {
  assert.deepEqual(
    actual.map(({ id }) => id),
    reference.map(({ id }) => id),
    'actual and reference operation order',
  );
  for (let index = 0; index < actual.length; index += 1) {
    assert.deepEqual(actual[index], reference[index], `${actual[index].id}: differential semantics`);
  }
}

export async function exerciseDeployedOperationSweep({
  actual,
  reference,
  coverage,
  coveragePath = defaultCoveragePath,
  fetchImpl = fetch,
}) {
  const contract = coverage ?? JSON.parse(fs.readFileSync(coveragePath, 'utf8'));
  const probes = buildDeployedProbePlan(contract);
  const run = async (target) => {
    assert.ok(target?.baseURL && target?.apiKey, `${target?.name ?? 'target'} credentials`);
    assert.ok(target.tunnelAccessToken, `${target.name} Tunnel bearer`);
    const results = [];
    for (const probe of probes) results.push(await executeProbe(probe, target, fetchImpl));
    return results;
  };
  const actualResults = await run(actual);
  if (reference) compareDeployedResults(actualResults, await run(reference));
  return actualResults;
}
