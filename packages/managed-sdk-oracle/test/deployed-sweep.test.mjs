import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import {
  assertManagedResponseContext,
  buildDeployedProbePlan,
  compareDeployedResults,
  exerciseDeployedOperationSweep,
} from '../src/conformance/deployed-sweep.mjs';

const operations = [
  { id: 'beta.sessions.list', method: 'GET', path: '/v1/sessions', resource: 'sessions', betas: ['managed-agents-2026-04-01'] },
  { id: 'beta.sessions.retrieve', method: 'GET', path: '/v1/sessions/{}', resource: 'sessions', betas: ['managed-agents-2026-04-01'] },
  { id: 'beta.sessions.create', method: 'POST', path: '/v1/sessions', resource: 'sessions', betas: ['managed-agents-2026-04-01'] },
  { id: 'beta.tunnels.retrieve', method: 'GET', path: '/v1/tunnels/{}', resource: 'tunnels', betas: ['mcp-tunnels-2026-06-22'] },
  { id: 'documented.organizationTunnels.list', method: 'GET', path: '/v1/organizations/tunnels', resource: 'tunnels', betas: ['mcp-tunnels-2026-05-19'] },
  { id: 'documented.skills.versions.retrieveFile', method: 'GET', path: '/v1/skills/{}/versions/{}/files/{}', transport_query: 'beta=true', resource: 'skills', betas: ['skills-2025-10-02'] },
  { id: 'beta.models.list', method: 'GET', path: '/v1/models', transport_query: 'beta=true', resource: 'models', betas: [] },
  { id: 'models.list', method: 'GET', path: '/v1/models', resource: 'models', betas: [] },
];
const coverage = { schema_version: 2, operations };
const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

test('response context is scalar, workspace-exact, unique, and non-retaining', () => {
  // Cause/effect graph:
  // one authenticated Workspace + one completed response
  //   -> exactly one request-id and one matching Workspace response field.
  //
  // Decision table:
  // | request field | workspace field | expected scope | prior id | effect |
  // | scalar        | exact           | configured     | absent   | accept |
  // | absent/joined | any             | any            | any      | reject |
  // | scalar        | absent/joined   | any            | any      | reject |
  // | scalar        | foreign         | configured     | absent   | reject |
  // | scalar        | exact           | configured     | present  | reject |
  // Only booleans leave the assertion, so successful evidence retains neither
  // response coordinate.
  const seenRequestIDs = new Set();
  const accepted = assertManagedResponseContext({
    requestID: ' req_one ',
    workspaceID: ' workspace_expected ',
    expectedWorkspaceID: 'workspace_expected',
    seenRequestIDs,
    label: 'accepted',
  });
  assert.deepEqual(accepted, { requestID: true, workspaceID: true });
  assert.ok(!JSON.stringify(accepted).includes('req_one'));
  assert.ok(!JSON.stringify(accepted).includes('workspace_expected'));
  for (const [label, requestID, workspaceID, expectedWorkspaceID, seen] of [
    ['missing request', undefined, 'workspace_expected', 'workspace_expected', new Set()],
    ['joined request', 'req_one, req_two', 'workspace_expected', 'workspace_expected', new Set()],
    ['missing workspace', 'req_two', undefined, 'workspace_expected', new Set()],
    ['joined workspace', 'req_two', 'workspace_expected, workspace_other', 'workspace_expected', new Set()],
    ['foreign workspace', 'req_two', 'workspace_other', 'workspace_expected', new Set()],
    ['reused request', 'req_one', 'workspace_expected', 'workspace_expected', seenRequestIDs],
  ]) {
    assert.throws(
      () => assertManagedResponseContext({
        requestID,
        workspaceID,
        expectedWorkspaceID,
        seenRequestIDs: seen,
        label,
      }),
      /official SDK response context|authenticated workspace|exactly one response/u,
      label,
    );
  }
});

test('deployed probe plan closes operations with one auth and beta policy', () => {
  // Cause/effect graph: C1 every generated operation enters the public sweep;
  // C2 current Tunnel routes select WIF while C3 legacy routes select the API
  // key; C4 item ids are deliberately absent. Effects: E1 exactly one probe
  // per operation, E2 no credential fallback, E3 deterministic 4xx semantics.
  // C5 permits a header-free Beta namespace only when the whole family selects
  // beta=true, every method omits the endpoint capability, and a GA surface
  // exists; a partial omission or missing GA family cannot masquerade as GA.
  const plan = buildDeployedProbePlan(coverage);
  assert.equal(plan.length, operations.length, 'C1/E1');
  assert.equal(new Set(plan.map(({ id }) => id)).size, operations.length, 'C1/E1');
  assert.equal(plan.find(({ id }) => id === 'beta.tunnels.retrieve').auth, 'bearer', 'C2/E2');
  assert.equal(
    plan.find(({ id }) => id === 'documented.organizationTunnels.list').auth,
    'api_key',
    'C3/E2',
  );
  assert.match(plan.find(({ id }) => id === 'beta.sessions.retrieve').path, /qualification-missing/u, 'C4/E3');
  const documentedSkill = plan.find(({ id }) => id === 'documented.skills.versions.retrieveFile');
  assert.equal(documentedSkill.beta, 'skills-2025-10-02', 'documented Beta capability');
  assert.match(documentedSkill.path, /\?beta=true$/u, 'documented Beta selector');
  assert.equal(plan.find(({ id }) => id === 'beta.models.list').beta, undefined, 'C5');
  assert.throws(
    () => buildDeployedProbePlan({
      schema_version: 2,
      operations: operations.filter(({ id }) => id !== 'models.list'),
    }),
    /beta\.models\.list: no public beta policy/u,
    'C5',
  );
});

test('generated Beta and GA inventory is executable by the deployed sweep', () => {
  // Partition contract: B=every client.beta operation carries beta=true;
  // G=every scoped GA operation carries no selector; D=reviewed SDK-absent
  // routes retain their explicit transport metadata. All partitions enter one
  // 139-operation plan without id or channel collapse.
  const generated = JSON.parse(fs.readFileSync(path.resolve(
    packageRoot,
    '../../contracts/anthropic-managed/operation-coverage.generated.json',
  )));
  const plan = buildDeployedProbePlan(generated);
  assert.equal(plan.length, generated.operations.length);
  assert.equal(new Set(plan.map(({ id }) => id)).size, plan.length);
  const beta = plan.filter(({ id }) => id.startsWith('beta.'));
  const ga = plan.filter(({ id }) => !id.startsWith('beta.') && !id.startsWith('documented.'));
  assert.equal(beta.length, 112);
  assert.equal(ga.length, 15);
  assert.ok(beta.every(({ path }) => path.endsWith('?beta=true')), 'B');
  assert.ok(ga.every(({ path }) => !path.includes('?beta=true')), 'G');
});

test('deployed sweep rejects malformed errors before compatibility is claimed', async () => {
  // Cause/effect graph: collection reads use a valid empty query; mutations use
  // a syntax-invalid JSON witness independent of optional SDK fields; item
  // reads use absent ids. Every negative response must cross the Anthropic
  // envelope and response-context boundaries. Decision table:
  // collection+200+page+both context headers => accept;
  // negative+4xx+Anthropic envelope+both context headers => accept;
  // mutation without exact malformed witness, negative+5xx, malformed envelope,
  // either missing context coordinate, wrong Workspace, or a reused request id
  // => fail closed. Exact identifiers are compared in memory and deliberately
  // excluded from the returned qualification evidence.
  const target = {
    name: 'actual',
    baseURL: 'https://actual.invalid',
    apiKey: 'key',
    tunnelAccessToken: 'token',
    workspaceId: 'workspace_fixture',
  };
  let requestSequence = 0;
  const validFetch = async (url, init) => {
    const operation = buildDeployedProbePlan(coverage).find(
      ({ method, path }) => init.method === method
        && url.pathname === new URL(path, target.baseURL).pathname,
    );
    assert.ok(operation);
    if (!['GET', 'HEAD', 'DELETE'].includes(init.method)) {
      assert.equal(init.headers['content-type'], 'application/json');
      assert.equal(init.body, '{', `${operation.id}: operation-independent malformed witness`);
    }
    requestSequence += 1;
    if (operation.expectedClass === 'collection') {
      return new Response(JSON.stringify({ data: [], has_more: false, next_page: null }), {
        status: 200,
        headers: {
          'content-type': 'application/json',
          'request-id': `req_fixture_${requestSequence}`,
          'anthropic-workspace-id': 'workspace_fixture',
        },
      });
    }
    assert.ok(init.headers['x-api-key'] || init.headers.authorization, 'one explicit auth policy');
    return new Response(JSON.stringify({
      type: 'error', error: { type: 'not_found_error', message: 'missing' },
    }), {
      status: 404,
      headers: {
        'content-type': 'application/json',
        'request-id': `req_fixture_${requestSequence}`,
        'anthropic-workspace-id': 'workspace_fixture',
      },
    });
  };
  const results = await exerciseDeployedOperationSweep({
    actual: target,
    coverage,
    fetchImpl: validFetch,
  });
  assert.equal(results.length, operations.length);
  assert.ok(results.every(({ responseContext }) => (
    responseContext.requestID && responseContext.workspaceID
  )));
  assert.ok(!JSON.stringify(results).includes('req_fixture_'));
  assert.ok(!JSON.stringify(results).includes(target.workspaceId));

  await assert.rejects(
    () => exerciseDeployedOperationSweep({
      actual: { ...target, workspaceId: ` ${target.workspaceId} ` },
      coverage,
      fetchImpl: validFetch,
    }),
    /Workspace identity/u,
    'an ambiguous configured Workspace cannot weaken exact response binding',
  );
  await assert.rejects(
    () => exerciseDeployedOperationSweep({
      actual: target,
      coverage,
      fetchImpl: validFetch,
      actualSeenRequestIDs: [],
    }),
    /request identity ledger/u,
    'the cross-phase uniqueness ledger has one closed representation',
  );

  // Metamorphic relation: splitting the same qualification run into ingress
  // and operation-sweep phases cannot reset request identity ownership. The
  // caller-provided ledger therefore accepts one sweep and rejects an
  // otherwise valid second sweep whose response ids restart from the same
  // sequence.
  const qualificationRequestIDs = new Set();
  requestSequence = 0;
  await exerciseDeployedOperationSweep({
    actual: target,
    coverage,
    fetchImpl: validFetch,
    actualSeenRequestIDs: qualificationRequestIDs,
  });
  requestSequence = 0;
  await assert.rejects(
    () => exerciseDeployedOperationSweep({
      actual: target,
      coverage,
      fetchImpl: validFetch,
      actualSeenRequestIDs: qualificationRequestIDs,
    }),
    /request-id must identify exactly one response/u,
    'request identity uniqueness spans every phase in one qualification run',
  );

  for (const missing of ['request-id', 'anthropic-workspace-id']) {
    await assert.rejects(
      () => exerciseDeployedOperationSweep({
        actual: target,
        coverage,
        fetchImpl: async () => new Response(JSON.stringify({
          data: [], has_more: false, next_page: null,
        }), {
          status: 200,
          headers: {
            'content-type': 'application/json',
            ...Object.fromEntries(
              ['request-id', 'anthropic-workspace-id']
                .filter((name) => name !== missing)
                .map((name) => [name, `${name}-fixture`]),
            ),
          },
        }),
      }),
      /official SDK response context/u,
      `${missing} is required for every operation partition`,
    );
  }

  await assert.rejects(
    () => exerciseDeployedOperationSweep({
      actual: target,
      coverage,
      fetchImpl: async () => jsonResponse(
        { data: [], has_more: false, next_page: null },
        200,
        { 'anthropic-workspace-id': 'workspace_foreign' },
      ),
    }),
    /response workspace must equal authenticated workspace/u,
    'a valid but foreign Workspace header cannot satisfy deployed compatibility',
  );

  let malformedSequence = 0;
  await assert.rejects(
    () => exerciseDeployedOperationSweep({
      actual: target,
      coverage,
      fetchImpl: async (url, init) => {
        const operation = buildDeployedProbePlan(coverage).find(
          ({ method, path }) => init.method === method
            && url.pathname === new URL(path, target.baseURL).pathname,
        );
        return operation.expectedClass === 'collection'
          ? jsonResponse({ data: [], has_more: false, next_page: null }, 200)
          : jsonResponse({
            type: 'error', error: { type: 'not_found_error', message: 'missing' },
          }, 404);
      },
    }),
    /request-id must identify exactly one response/u,
    'a proxy-wide constant request id cannot satisfy deployed compatibility',
  );

  await assert.rejects(
    () => exerciseDeployedOperationSweep({
      actual: target,
      coverage,
      fetchImpl: async (url, init) => {
        const operation = buildDeployedProbePlan(coverage).find(
          ({ method, path }) => init.method === method
            && url.pathname === new URL(path, target.baseURL).pathname,
        );
        malformedSequence += 1;
        return operation.expectedClass === 'collection'
          ? jsonResponse(
            { data: [], has_more: false, next_page: null },
            200,
            { 'request-id': `req_malformed_${malformedSequence}` },
          )
          : jsonResponse({}, 404, { 'request-id': `req_malformed_${malformedSequence}` });
      },
    }),
    /error envelope type/u,
  );

  let mediaSequence = 0;
  await assert.rejects(
    () => exerciseDeployedOperationSweep({
      actual: target,
      coverage,
      fetchImpl: async (url, init) => {
        const operation = buildDeployedProbePlan(coverage).find(
          ({ method, path }) => init.method === method
            && url.pathname === new URL(path, target.baseURL).pathname,
        );
        const body = operation.expectedClass === 'collection'
          ? { data: [], has_more: false, next_page: null }
          : { type: 'error', error: { type: 'not_found_error', message: 'missing' } };
        mediaSequence += 1;
        return new Response(JSON.stringify(body), {
          status: operation.expectedClass === 'collection' ? 200 : 404,
          headers: {
            'request-id': `req_media_${mediaSequence}`,
            'anthropic-workspace-id': 'workspace_fixture',
          },
        });
      },
    }),
    /expected JSON media type/u,
    'JSON bytes without the protocol media type are not SDK compatibility evidence',
  );
});

function jsonResponse(body, status, headers = {}) {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      'content-type': 'application/json',
      'request-id': 'req_fixture',
      'anthropic-workspace-id': 'workspace_fixture',
      ...headers,
    },
  });
}

test('official reference differential compares status and stable response shape', () => {
  // Causes: C1 nondeterministic values are absent from the shape evidence; C2
  // status, pagination fields, or error kind drift. Effect: C1 compares equal,
  // while every C2 partition fails with the exact operation id.
  const actual = [{
    id: 'beta.sessions.retrieve',
    status: 404,
    responseContext: { requestID: true, workspaceID: true },
    shape: {
    keys: ['error', 'type'], error: {
      keys: ['message', 'type'], type: 'not_found_error', message: 'missing session',
    },
  } }];
  assert.doesNotThrow(() => compareDeployedResults(actual, structuredClone(actual)), 'C1');
  const drift = structuredClone(actual);
  drift[0].status = 400;
  assert.throws(() => compareDeployedResults(actual, drift), /beta\.sessions\.retrieve/u, 'C2/status');
});
