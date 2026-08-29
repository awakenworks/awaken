import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import test from 'node:test';
import {
  MANAGED_SDK_RESPONSE_FINGERPRINT_KEY,
  assertOwnerOperationReceipts,
  assertResponseContract,
  managedSdkReceipt,
  recordingFetch,
  receiptMatchesOperation,
} from './managed_sdk_operation_receipts.mjs';

const betaOperation = {
  sdkMethod: 'beta.files.retrieveMetadata',
  owner: 'files.mjs',
  method: 'GET',
  route: '/v1/files/{}',
  transportQuery: 'beta=true',
  betas: ['files-api-2025-04-14'],
  responseContract: {
    kind: 'json',
    schema: {
      kind: 'object',
      properties: {
        id: { required: true, value: { kind: 'string' } },
        downloadable: { required: false, value: { kind: 'boolean' } },
      },
      additional: false,
    },
  },
};
const gaOperation = {
  sdkMethod: 'files.retrieveMetadata',
  owner: 'files.mjs',
  method: 'GET',
  route: '/v1/files/{}',
  betas: [],
  responseContract: betaOperation.responseContract,
};
const betaReceipt = {
  method: 'GET',
  path: '/v1/files/file_1',
  beta: 'true',
  betas: ['files-api-2025-04-14'],
  sdk: true,
  sdkVersion: '0.121.0',
  status: 200,
  responseContext: { requestID: true, workspaceID: true },
  responseShape: { kind: 'object', fields: { id: { kind: 'string' } } },
};
const sdkVersion = '0.121.0';

test('runtime receipts distinguish Beta and GA calls sharing one route', () => {
  // Cause/effect graph: C1=method/path are equal; C2=query selector differs;
  // C3=capability header differs. Effects: E1 the Beta receipt owns only the
  // Beta operation; E2 omitting either discriminator cannot certify GA as Beta
  // or Beta as GA. Decision table: C1+C2+C3=>E1; C1+(!C2||!C3)=>E2.
  assert.equal(receiptMatchesOperation(betaReceipt, betaOperation, sdkVersion), true, 'C1+C2+C3/E1');
  assert.equal(receiptMatchesOperation(betaReceipt, gaOperation, sdkVersion), false, 'C1+C2/E2');
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, beta: null },
    betaOperation,
    sdkVersion,
  ), false, 'C1+!C2/E2');
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, betas: [] },
    betaOperation,
    sdkVersion,
  ), false, 'C1+!C3/E2');
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, betas: [...betaReceipt.betas, 'managed-agents-2026-04-01'] },
    betaOperation,
    sdkVersion,
  ), true, 'orthogonal capabilities compose without changing operation ownership');

  const queryBetaOperation = {
    ...betaOperation,
    betas: [],
    forbiddenBetas: ['files-api-2025-04-14'],
  };
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, betas: [] }, queryBetaOperation, sdkVersion,
  ), true, 'post-GA generated request owns the query-only transport');
  assert.equal(receiptMatchesOperation(
    betaReceipt, queryBetaOperation, sdkVersion,
  ), false, 'a caller-injected legacy capability cannot mask generated transport drift');
});

test('only an official SDK request can satisfy executable ownership', () => {
  // Cause: a direct fetch can duplicate the public method, route and headers.
  // Effect: the Stainless SDK marker remains required, so source text plus a
  // hand-written fetch cannot masquerade as an official SDK behavior proof.
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, sdk: false },
    betaOperation,
    sdkVersion,
  ), false);
});

test('an adjacent SDK version cannot satisfy executable ownership', () => {
  // Metamorphic relation: preserve method, path, selectors, response and SDK
  // marker while changing only x-stainless-package-version. Ownership must
  // flip from true to false, preventing a current-package execution from being
  // reported as candidate evidence by the orchestration layer.
  assert.equal(receiptMatchesOperation(betaReceipt, betaOperation, '0.121.0'), true);
  assert.equal(receiptMatchesOperation(betaReceipt, betaOperation, '0.122.0'), false);
});

test('only a successful application response is positive operation evidence', () => {
  // Cause/effect partitions: no response, a 2xx success, a 4xx application
  // rejection, and a 5xx server fault. Only 2xx proves the advertised operation
  // can complete. Error taxonomy has its own differential matrix, so accepting
  // 4xx here would let an implementation reject every call while appearing to
  // implement the entire SDK surface.
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, status: undefined }, betaOperation, sdkVersion,
  ), false);
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, status: 200 }, betaOperation, sdkVersion,
  ), true);
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, status: 404 }, betaOperation, sdkVersion,
  ), false);
  assert.equal(receiptMatchesOperation(
    { ...betaReceipt, status: 500 }, betaOperation, sdkVersion,
  ), false);
});

test('every successful operation receipt requires the official SDK response context', () => {
  // Cause/effect graph: a real official SDK request reaches one operation,
  // then router composition must project both correlation and Workspace
  // coordinates before the response crosses back into the SDK. A 2xx DTO by
  // itself cannot prove that complete call chain.
  //
  // Decision table:
  // | 2xx | request-id | workspace-id | ownership evidence |
  // | yes | present    | present      | accepted           |
  // | yes | absent     | present      | rejected           |
  // | yes | present    | absent       | rejected           |
  // | no  | any        | any          | rejected elsewhere |
  assert.equal(receiptMatchesOperation(betaReceipt, betaOperation, sdkVersion), true);
  assert.equal(receiptMatchesOperation({
    ...betaReceipt,
    responseContext: { requestID: false, workspaceID: true },
  }, betaOperation, sdkVersion), false);
  assert.equal(receiptMatchesOperation({
    ...betaReceipt,
    responseContext: { requestID: true, workspaceID: false },
  }, betaOperation, sdkVersion), false);
  assert.equal(receiptMatchesOperation({
    ...betaReceipt,
    responseContext: undefined,
  }, betaOperation, sdkVersion), false, 'legacy/partial receipts fail closed');
});

test('owner qualification fails closed for every missing runtime edge', () => {
  // Decision table: R1 every owned operation has a matching runtime receipt ->
  // count all operations; R2 one receipt is absent or belongs to another owner
  // -> fail with the exact uncovered method. Extra support traffic is allowed
  // because lifecycle setup may call operations owned by another scenario.
  const manifest = [betaOperation, gaOperation];
  const gaReceipt = { ...betaReceipt, beta: null, betas: [] };
  assert.equal(
    assertOwnerOperationReceipts(manifest, 'files.mjs', [betaReceipt, gaReceipt], sdkVersion),
    2,
    'R1',
  );
  assert.throws(
    () => assertOwnerOperationReceipts(manifest, 'files.mjs', [betaReceipt], sdkVersion),
    /files\.retrieveMetadata/u,
    'R2',
  );
  assert.throws(
    () => assertOwnerOperationReceipts(manifest, 'files.mjs', [], sdkVersion),
    (error) => error.message.includes('beta.files.retrieveMetadata')
      && error.message.includes('files.retrieveMetadata'),
    'R2 reports every missing edge in one run',
  );
});

test('official response contract rejects missing, extra, primitive, and nested drift', () => {
  // Causal graph: C1 the exact SDK declaration owns required/optional fields,
  // nesting, unions and closed object boundaries; C2 the runtime receipt owns
  // a structural shape plus non-reversible literal fingerprints. Effects:
  // matching values pass while a
  // missing required field, undeclared extension, wrong primitive or nested
  // mutation fails. This is the executable bridge from generated SDK types to
  // every real 2xx owner and cannot leak a credential value into evidence.
  const contract = {
    kind: 'json',
    schema: {
      kind: 'object',
      properties: {
        id: { required: true, value: { kind: 'string' } },
        detail: {
          required: true,
          value: {
            kind: 'object',
            properties: {
              enabled: { required: true, value: { kind: 'boolean' } },
              note: {
                required: false,
                value: { kind: 'union', variants: [{ kind: 'string' }, { kind: 'null' }] },
              },
            },
            additional: false,
          },
        },
      },
      additional: false,
    },
  };
  const valid = {
    kind: 'object',
    fields: {
      detail: { kind: 'object', fields: { enabled: { kind: 'boolean' } } },
      id: { kind: 'string' },
    },
  };
  assert.doesNotThrow(() => assertResponseContract(valid, contract, 'valid'));
  for (const [label, mutate, message] of [
    ['missing', (shape) => { delete shape.fields.id; }, /required field is absent/u],
    ['extra', (shape) => { shape.fields.extension = { kind: 'string' }; }, /not in the official type/u],
    ['primitive', (shape) => { shape.fields.id = { kind: 'number' }; }, /expected string/u],
    ['nested', (shape) => { shape.fields.detail.fields.enabled = { kind: 'string' }; }, /expected boolean/u],
  ]) {
    const shape = structuredClone(valid);
    mutate(shape);
    assert.throws(() => assertResponseContract(shape, contract, label), message, label);
  }
});

test('intent-named open JSON does not weaken its enclosing response contract', () => {
  // Cause/effect graph: C1 official tool input is schema-defined per Agent and
  // therefore intentionally open; C2 its enclosing event DTO remains closed.
  // Effects: E1 arbitrary nested JSON is accepted only at `input`; E2 an extra
  // sibling or wrong enclosing primitive still fails. Decision table:
  // C1+C2 -> E1; open payload + violated C2 -> E2. This proves `open-json` is a
  // local protocol extension point, not a free-form escape hatch for the DTO.
  const contract = {
    kind: 'json',
    schema: {
      kind: 'object',
      properties: {
        id: { required: true, value: { kind: 'string' } },
        input: { required: true, value: { kind: 'open-json', purpose: 'tool-input' } },
      },
      additional: false,
    },
  };
  const input = {
    kind: 'object',
    fields: {
      nested: { kind: 'array', items: [{ kind: 'boolean' }, { kind: 'null' }] },
    },
  };
  assert.doesNotThrow(() => assertResponseContract({
    kind: 'object',
    fields: { id: { kind: 'string' }, input },
  }, contract, 'tool event'), 'C1+C2/E1');
  assert.throws(() => assertResponseContract({
    kind: 'object',
    fields: { id: { kind: 'string' }, input, leaked: { kind: 'string' } },
  }, contract, 'tool event'), /field is not in the official type/u, 'C1+!C2/E2');
});

test('official literal contracts reject a wrong discriminator without retaining its value', async () => {
  // Causal graph: C1 the exact SDK declaration supplies a finite literal; C2
  // the completed JSON response supplies the observed value; C3 the receipt
  // crosses a child-process boundary. E1 equal values pass, E2 a same-primitive
  // but different value fails, and E3 neither value appears in serialized
  // evidence. This closes the gap where `"session"` was formerly only checked
  // as `string` while preserving credential and user-data non-interference.
  const request = new Request('https://managed.invalid/v1/fixture', {
    headers: { 'x-stainless-lang': 'js', 'x-stainless-package-version': sdkVersion },
  });
  const receipt = await managedSdkReceipt(request, undefined, new Response(
    JSON.stringify({ type: 'session' }),
    { headers: { 'content-type': 'application/json' } },
  ));
  const contract = (value) => ({
    kind: 'json',
    schema: {
      kind: 'object',
      properties: {
        type: {
          required: true,
          value: { kind: 'literal', primitive: 'string', value },
        },
      },
      additional: false,
    },
  });
  assert.doesNotThrow(() => assertResponseContract(receipt.responseShape, contract('session'), 'equal'));
  assert.throws(
    () => assertResponseContract(receipt.responseShape, contract('dream'), 'different'),
    /expected literal "dream"/u,
  );
  const serialized = JSON.stringify(receipt);
  assert.ok(!serialized.includes('session'));
  assert.ok(!serialized.includes('dream'));
});

test('page ownership requires at least one observed official item shape', () => {
  // Causal graph: an empty page proves only the pagination envelope; a non-empty
  // page additionally proves the SDK element DTO. Evidence is aggregated across
  // repeated calls so legitimate empty filters remain allowed once one real item
  // has crossed the same operation. This closes the vacuous-array loophole
  // without requiring every successful list response to be non-empty.
  const pageOperation = {
    ...gaOperation,
    sdkMethod: 'files.list',
    route: '/v1/files',
    responseContract: {
      kind: 'json',
      schema: {
        kind: 'object',
        properties: {
          data: {
            required: true,
            value: {
              kind: 'array',
              item: {
                kind: 'object',
                properties: { id: { required: true, value: { kind: 'string' } } },
                additional: false,
              },
            },
          },
        },
        additional: false,
      },
      evidence: { nonEmptyArrays: [['data']] },
    },
  };
  const empty = {
    ...betaReceipt,
    path: '/v1/files',
    beta: null,
    betas: [],
    responseShape: { kind: 'object', fields: { data: { kind: 'array', items: [] } } },
  };
  assert.throws(
    () => assertOwnerOperationReceipts([pageOperation], 'files.mjs', [empty], sdkVersion),
    /item shape at \$\.data was never observed/u,
  );
  const populated = {
    ...empty,
    responseShape: {
      kind: 'object',
      fields: {
        data: {
          kind: 'array',
          items: [{ kind: 'object', fields: { id: { kind: 'string' } } }],
        },
      },
    },
  };
  assert.equal(
    assertOwnerOperationReceipts([pageOperation], 'files.mjs', [empty, populated], sdkVersion),
    1,
  );
});

test('an identical historical request projection is bounded by the current additive wire contract', () => {
  // Version-axis graph: C1 the old package owns request generation and its media
  // class; C2 identical request coordinates select one current wire projection.
  // E1 a reviewed canonical addition is accepted, E2 an arbitrary
  // addition is still rejected, and E3 a JSON/binary/stream change fails before
  // shape validation. This preserves the no-User-Agent-versioning invariant.
  const operation = {
    ...gaOperation,
    responseContract: {
      kind: 'json',
      schema: {
        kind: 'object',
        properties: { id: { required: true, value: { kind: 'string' } } },
        additional: false,
      },
    },
    wireResponseContract: {
      kind: 'json',
      schema: {
        kind: 'object',
        properties: {
          id: { required: true, value: { kind: 'string' } },
          reviewed: { required: true, value: { kind: 'boolean' } },
        },
        additional: false,
      },
    },
  };
  const receipt = {
    ...betaReceipt,
    beta: null,
    betas: [],
    responseShape: {
      kind: 'object',
      fields: { id: { kind: 'string' }, reviewed: { kind: 'boolean' } },
    },
  };
  assert.equal(
    assertOwnerOperationReceipts([operation], 'files.mjs', [receipt], sdkVersion),
    1,
    'E1',
  );
  assert.throws(
    () => assertOwnerOperationReceipts(
      [operation],
      'files.mjs',
      [{
        ...receipt,
        responseShape: {
          ...receipt.responseShape,
          fields: { ...receipt.responseShape.fields, arbitrary: { kind: 'string' } },
        },
      }],
      sdkVersion,
    ),
    /field is not in the official type/u,
    'E2',
  );
  assert.throws(
    () => assertOwnerOperationReceipts(
      [{ ...operation, wireResponseContract: { kind: 'binary' } }],
      'files.mjs',
      [receipt],
      sdkVersion,
    ),
    /media disagree/u,
    'E3',
  );
});

test('response shape partitions JSON arrays, binary, stream, and empty bodies without values', async () => {
  // Decision table: JSON records recursive unique item shapes; octet streams
  // remain binary; SSE is never consumed by instrumentation; 204 is empty.
  // The literal secret below is deliberately absent from every receipt.
  const request = new Request('https://managed.invalid/v1/fixture', {
    headers: { 'x-stainless-lang': 'js', 'x-stainless-package-version': sdkVersion },
  });
  const cases = [
    new Response(JSON.stringify([{ token: 'must-not-leak' }, { token: 'another-secret' }]), { // awaken-allow: secret
      headers: { 'content-type': 'application/json; charset=utf-8' },
    }),
    new Response('bytes', { headers: { 'content-type': 'application/octet-stream' } }),
    new Response('data: never-read\n\n', { headers: { 'content-type': 'text/event-stream' } }),
    new Response(null, { status: 204 }),
  ];
  const expectedKinds = ['array', 'binary', 'stream', 'empty'];
  for (const [index, response] of cases.entries()) {
    const receipt = await managedSdkReceipt(request, undefined, response);
    assert.equal(receipt.responseShape.kind, expectedKinds[index]);
    assert.ok(!JSON.stringify(receipt).includes('must-not-leak'));
    assert.ok(!JSON.stringify(receipt).includes('another-secret'));
    assert.ok(!JSON.stringify(receipt).includes('never-read'));
    if (index === 0) {
      assert.equal(receipt.responseShape.items.length, 2, 'distinct literal observations are retained');
      for (const item of receipt.responseShape.items) {
        assert.equal(item.fields.token.kind, 'string');
        assert.match(item.fields.token.fingerprint, /^[0-9a-f]{64}$/u);
      }
    }
  }
});

test('path placeholders match one non-empty segment and no broader route', () => {
  // Boundary partition: one encoded ID is valid; missing, extra and nested
  // path segments are invalid. This prevents a nearby endpoint from producing
  // a false receipt for the owned operation.
  assert.equal(receiptMatchesOperation(betaReceipt, betaOperation, sdkVersion), true);
  for (const path of ['/v1/files/', '/v1/files', '/v1/files/a/extra']) {
    assert.equal(receiptMatchesOperation(
      { ...betaReceipt, path }, betaOperation, sdkVersion,
    ), false, path);
  }
});

test('static subresources outrank placeholder identities during receipt attribution', () => {
  // Causal graph: `/work/stats` satisfies the raw `/work/{work_id}` grammar,
  // but the SDK call targeted the longer static route. The most-specific route
  // must own the receipt; otherwise one stats response can falsely satisfy
  // retrieve and then be validated against the wrong DTO.
  const responseContract = {
    kind: 'json',
    schema: { kind: 'object', properties: {}, additional: false },
  };
  const retrieve = {
    sdkMethod: 'beta.environments.work.retrieve',
    owner: 'work.mjs',
    method: 'GET',
    route: '/v1/environments/{}/work/{}',
    betas: ['managed-agents-2026-04-01'],
    responseContract,
  };
  const stats = {
    ...retrieve,
    sdkMethod: 'beta.environments.work.stats',
    route: '/v1/environments/{}/work/stats',
  };
  const receipt = {
    ...betaReceipt,
    path: '/v1/environments/env_1/work/stats',
    beta: null,
    betas: ['managed-agents-2026-04-01'],
    responseShape: { kind: 'object', fields: {} },
  };
  assert.throws(
    () => assertOwnerOperationReceipts([retrieve, stats], 'work.mjs', [receipt], sdkVersion),
    /beta\.environments\.work\.retrieve/u,
    'the missing retrieve remains visible',
  );
  assert.equal(
    assertOwnerOperationReceipts(
      [retrieve, stats],
      'work.mjs',
      [receipt, { ...receipt, path: '/v1/environments/env_1/work/work_1' }],
      sdkVersion,
    ),
    2,
  );
});

test('transport hook records only non-secret completed exchange coordinates', () => {
  // Information-flow rule: credentials and bodies may enter the request, but
  // only method/path/selectors/SDK marker/status plus response-header presence
  // may enter the receipt; identifier values remain absent. The response status
  // is written after fetch completes, so an attempted request cannot leave an
  // apparently successful ownership fact.
  const directory = mkdtempSync(resolve(tmpdir(), 'awaken-receipt-hook-test-'));
  const receiptFile = resolve(directory, 'receipt.jsonl');
  try {
    execFileSync(process.execPath, [
      '--import', resolve(import.meta.dirname, 'managed_sdk_receipt_hook.mjs'),
      '--input-type=module',
      '--eval',
      "await fetch('data:application/json,%7B%7D', { headers: {"
        + " 'x-api-key': 'must-not-leak', 'x-stainless-lang': 'js'," // awaken-allow: secret
        + " 'anthropic-beta': 'one,two' } })",
    ], {
      env: {
        ...process.env,
        AWAKEN_MANAGED_SDK_RECEIPT_FILE: receiptFile,
        AWAKEN_MANAGED_SDK_RESPONSE_FINGERPRINT_KEY: MANAGED_SDK_RESPONSE_FINGERPRINT_KEY,
      },
    });
    const serialized = readFileSync(receiptFile, 'utf8');
    assert.ok(!serialized.includes('must-not-leak'), 'credential non-interference');
    assert.deepEqual(JSON.parse(serialized), {
      method: 'GET',
      path: 'application/json,%7B%7D',
      beta: null,
      betas: ['one', 'two'],
      sdk: true,
      sdkVersion: null,
      status: 200,
      responseContext: { requestID: false, workspaceID: false },
      responseShape: { kind: 'object', fields: {} },
    });
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test('in-process and child-process receipt collection share one encoder', async () => {
  // Cause/effect graph: C1 a candidate canary supplies a local fetch adapter;
  // C2 the behavior-owner gate installs the process hook. Both must project the
  // same request/response coordinates or one proof path could accept behavior
  // rejected by the other. The wrapper also must return the original Response
  // object so instrumentation cannot change SDK decoding semantics.
  const response = new Response('{}', {
    status: 200,
    headers: {
      'content-type': 'application/json',
      'request-id': 'req_fixture',
      'anthropic-workspace-id': 'workspace_fixture',
    },
  });
  const receipts = [];
  const input = new Request('https://managed.invalid/v1/files/file_1?beta=true', {
    headers: {
      'anthropic-beta': 'files-api-2025-04-14',
      'x-api-key': 'must-not-leak', // awaken-allow: secret
      'x-stainless-lang': 'js',
      'x-stainless-package-version': sdkVersion,
    },
  });
  const fetch = recordingFetch(async () => response, (receipt) => receipts.push(receipt));
  assert.equal(await fetch(input), response, 'instrumentation preserves response identity');
  assert.deepEqual(receipts, [await managedSdkReceipt(input, undefined, response)]);
  assert.deepEqual(receipts[0], {
    ...betaReceipt,
    status: 200,
    responseShape: { kind: 'object', fields: {} },
  });
  assert.ok(!JSON.stringify(receipts).includes('must-not-leak'), 'credential non-interference');
  assert.ok(!JSON.stringify(receipts).includes('req_fixture'), 'request identity non-interference');
  assert.ok(
    !JSON.stringify(receipts).includes('workspace_fixture'),
    'workspace identity non-interference',
  );
});

test('finite receipt model accepts exactly the conjunction of all ownership coordinates', () => {
  // Finite model check over the ten independent predicates in the ownership
  // invariant. Exhausting 2^10 combinations proves no single missing coordinate,
  // stale operation-local capability, or interaction can satisfy the matcher.
  const dimensions = [true, false];
  const operation = { ...betaOperation, forbiddenBetas: ['skills-2025-10-02'] };
  let cases = 0;
  for (const sdk of dimensions) {
    for (const healthy of dimensions) {
      for (const method of dimensions) {
        for (const path of dimensions) {
          for (const selector of dimensions) {
            for (const capability of dimensions) {
              for (const version of dimensions) {
                for (const noForbiddenCapability of dimensions) {
                  for (const requestContext of dimensions) {
                    for (const workspaceContext of dimensions) {
                      const receipt = {
                        ...betaReceipt,
                        sdk,
                        sdkVersion: version ? sdkVersion : '0.122.0',
                        status: healthy ? 200 : 500,
                        responseContext: {
                          requestID: requestContext,
                          workspaceID: workspaceContext,
                        },
                        method: method ? 'GET' : 'POST',
                        path: path ? '/v1/files/file_1' : '/v1/files/file_1/extra',
                        beta: selector ? 'true' : null,
                        betas: [
                          ...(capability ? ['files-api-2025-04-14'] : []),
                          ...(noForbiddenCapability ? [] : ['skills-2025-10-02']),
                        ],
                      };
                      assert.equal(
                        receiptMatchesOperation(receipt, operation, sdkVersion),
                        sdk && healthy && method && path && selector && capability
                          && version && noForbiddenCapability && requestContext
                          && workspaceContext,
                        JSON.stringify(receipt),
                      );
                      cases += 1;
                    }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
  assert.equal(cases, 1024);
});
