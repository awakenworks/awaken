// Shared request-contract witness runner; not a standalone E2E entry point.
import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';

import {
  assertRequestInvocationWitnessCoverage,
  requestInvocationWitnesses,
} from '../../packages/managed-sdk-oracle/src/request-witnesses.mjs';

const BASE_URL = 'https://managed-request.invalid';
const GROUP_PARAMETERS = new Set(['body', 'params', 'query']);
const DECLARED_RUNTIME_REJECTIONS = new Set([
  'beta.skills.create\0display_name',
  'skills.create\0display_name',
]);

function resourceMethod(client, operationID) {
  const parts = operationID.split('.');
  let resource = client;
  for (const part of parts.slice(0, -1)) resource = resource[part];
  return resource[parts.at(-1)].bind(resource);
}

function isUploadWitness(value) {
  return value && typeof value === 'object' && Object.hasOwn(value, '__managed_sdk_upload__');
}

function materialize(value) {
  if (isUploadWitness(value)) {
    const upload = value.__managed_sdk_upload__;
    return new File(
      [Buffer.from(upload.content_base64, 'base64')],
      upload.filename,
      { type: upload.media_type },
    );
  }
  if (Array.isArray(value)) return value.map(materialize);
  if (value && typeof value === 'object') {
    return Object.fromEntries(Object.entries(value).map(([name, nested]) => [name, materialize(nested)]));
  }
  return value;
}

function serializedPythonArguments(contract, invocation) {
  const arguments_ = [];
  for (const [index, parameter] of contract.parameters.entries()) {
    if (index >= invocation.length || invocation[index] === undefined) continue;
    const value = invocation[index];
    if (GROUP_PARAMETERS.has(parameter.name)) {
      if (value === null) continue;
      assert.equal(typeof value, 'object', `${parameter.name}: grouped request argument`);
      assert.ok(!Array.isArray(value), `${parameter.name}: grouped request argument is not an array`);
      for (const [name, nested] of Object.entries(value)) arguments_.push([name, nested]);
    } else {
      arguments_.push([parameter.name, value]);
    }
  }
  const normalized = arguments_.map(([name]) => name.replaceAll(/[^a-z0-9]/giu, '').toLowerCase());
  assert.equal(new Set(normalized).size, normalized.length, 'Python argument projection is injective');
  return arguments_;
}

function pythonEmptyPathRejection(arguments_, error, operation) {
  // The actual generated SDK owns path binding, including Environment ids
  // extracted from params. Its exact path diagnostic plus the one-factor
  // argument projection identifies the rejected field; body/query emptiness
  // alone admits nothing. No path mapper or fictitious HTTP request is needed.
  if (!/^Path parameters result in path with invalid segments:[\s\S]*Value "" can't be safely passed as a path parameter[\s\S]*\^/u.test(error.message)) return null;
  assert.equal(error.constructor.name, 'AnthropicError', `${operation.id}: exact empty-path error`);
  const fields = arguments_.filter(([, value]) => value === '').map(([name]) => name);
  assert.equal(fields.length, 1, `${operation.id}: one-factor empty path witness`);
  return {
    error_class: 'ValueError',
    field: fields[0],
  };
}

function sortedQuery(url) {
  return [...url.searchParams.entries()].sort(([leftName, leftValue], [rightName, rightValue]) =>
    leftName.localeCompare(rightName) || leftValue.localeCompare(rightValue));
}

function semanticHeaders(headers) {
  const betas = headers.get('anthropic-beta')
    ?.split(',')
    .map((value) => value.trim())
    .filter(Boolean)
    .sort() ?? [];
  return {
    accept: headers.get('accept'),
    anthropic_beta: betas,
    anthropic_worker_id: headers.get('anthropic-worker-id'),
  };
}

async function multipartBody(request) {
  const parts = [];
  for (const [name, value] of await request.formData()) {
    if (typeof value === 'string') {
      parts.push({ name, text: value });
    } else {
      parts.push({
        content_base64: Buffer.from(await value.arrayBuffer()).toString('base64'),
        filename: value.name,
        media_type: value.type || null,
        name,
      });
    }
  }
  return parts.sort((left, right) => JSON.stringify(left).localeCompare(JSON.stringify(right)));
}

async function semanticBody(request) {
  if (request.body === null) return { kind: 'empty' };
  const contentType = request.headers.get('content-type') ?? '';
  if (contentType.startsWith('multipart/form-data')) {
    return { kind: 'multipart', parts: await multipartBody(request) };
  }
  const bytes = Buffer.from(await request.arrayBuffer());
  if (bytes.length === 0) return { kind: 'empty' };
  if (contentType.startsWith('application/json')) {
    return { kind: 'json', value: JSON.parse(bytes.toString('utf8')) };
  }
  return { kind: 'binary', content_base64: bytes.toString('base64') };
}

async function semanticRequest(request) {
  const url = new URL(request.url);
  return {
    body: await semanticBody(request),
    headers: semanticHeaders(request.headers),
    method: request.method,
    path: decodeURIComponent(url.pathname),
    query: sortedQuery(url),
  };
}

export async function buildRequestWitnessBundle({ packageRoot, operations, contracts }) {
  // End-to-end request-construction causal graph:
  //
  // official generated JS operation inventory
  //   + adjacent declaration's finite request graph
  //   -> required-only baseline
  //   -> one-at-a-time union/null/optional/array/map/upload witnesses
  //   -> official SDK method + APIPromise.asResponse()
  //   -> WHATWG Request (path/query/header/JSON/multipart)
  //
  // Decision rules: every admitted finite branch emits exactly one request;
  // an empty path identity is rejected before transport in both runtimes;
  // a nullable multipart field retains its exact reviewed SDK variance.
  // Synchronous method construction and async response failures share this
  // rejection boundary. Data: capability probes are not API requests; RequestOptions
  // are orthogonal client transport controls; values are retained only in the
  // ephemeral bundle consumed by the exact Python wheel. This tests the public
  // developer call surface and the complete official transform/serialization
  // chain without copying a generated method or maintaining 127 fixtures.
  const { default: Anthropic } = await import(pathToFileURL(`${packageRoot}/index.mjs`));
  const captured = [];
  const fetch = async (input, init) => {
    const request = new Request(input, init);
    if (new URL(request.url).origin === BASE_URL) {
      captured.push(await semanticRequest(request.clone()));
    }
    return new Response('{}', { status: 200, headers: { 'content-type': 'application/json' } });
  };
  const client = new Anthropic({
    apiKey: 'request-contract', // awaken-allow: secret
    baseURL: BASE_URL,
    fetch,
    maxRetries: 0,
  });
  const witnesses = [];
  const upstreamRejections = [];
  for (const operation of operations) {
    const contract = contracts[operation.id];
    assert.ok(contract, `${operation.id}: request contract`);
    assertRequestInvocationWitnessCoverage(contract);
    const method = resourceMethod(client, operation.id);
    for (const invocation of requestInvocationWitnesses(contract)) {
      const before = captured.length;
      const arguments_ = serializedPythonArguments(contract, invocation);
      let response;
      try {
        const promise = method(...invocation.map(materialize));
        assert.equal(typeof promise.asResponse, 'function', `${operation.id}: APIPromise raw response`);
        response = await promise.asResponse();
      } catch (error) {
        assert.equal(captured.length, before, `${operation.id}: rejected before transport`);
        const pathRejection = pythonEmptyPathRejection(arguments_, error, operation);
        if (pathRejection) {
          witnesses.push({ arguments: arguments_, expected: null, operation_id: operation.id,
            python_rejection: pathRejection });
          continue;
        }
        const nullFields = arguments_
          .filter(([, value]) => value === null)
          .map(([name]) => `${operation.id}\0${name}`);
        assert.equal(nullFields.length, 1, `${operation.id}: exact rejected null field`);
        assert.ok(
          DECLARED_RUNTIME_REJECTIONS.has(nullFields[0]),
          `${operation.id}: unreviewed declaration/runtime rejection: ${error.message}`,
        );
        assert.match(error.message, /Received null for "[^"]+"/u, operation.id);
        const omitted = arguments_.filter(([, value]) => value !== null);
        const omissionWitness = witnesses.find((candidate) =>
          candidate.operation_id === operation.id
          && JSON.stringify(candidate.arguments) === JSON.stringify(omitted));
        assert.ok(omissionWitness, `${operation.id}: nullable multipart omission witness`);
        upstreamRejections.push({
          arguments: arguments_,
          error_class: error.constructor.name,
          error_message: error.message,
          operation_id: operation.id,
          python_expected: omissionWitness.expected,
        });
        continue;
      }
      assert.equal(response.status, 200, operation.id);
      assert.equal(captured.length, before + 1, `${operation.id}: exact API request count`);
      const expected = captured.at(-1);
      assert.equal(expected.method, operation.method, `${operation.id}: method`);
      witnesses.push({
        arguments: arguments_,
        expected,
        operation_id: operation.id,
        python_rejection: null,
      });
    }
  }
  assert.equal(
    new Set(witnesses.map(({ operation_id }) => operation_id)).size,
    operations.length,
    'every operation emits request witnesses',
  );
  assert.deepEqual(
    new Set(upstreamRejections.map(({ operation_id, arguments: arguments_ }) => {
      const [name] = arguments_.find(([, value]) => value === null);
      return `${operation_id}\0${name}`;
    })),
    DECLARED_RUNTIME_REJECTIONS,
    'the exact reviewed upstream declaration/runtime rejection set',
  );
  return { upstream_rejections: upstreamRejections, witnesses };
}

export const requestContractInternals = Object.freeze({
  materialize,
  pythonEmptyPathRejection,
  semanticHeaders,
  serializedPythonArguments,
});
