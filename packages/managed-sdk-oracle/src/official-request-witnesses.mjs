// Official request witnesses: for every current Managed operation, the exact
// wire request (method, path, query, JSON/multipart body) the official SDK
// emits for one minimal (required-only) invocation, one maximal (every optional
// present) invocation, and one invocation per union member reachable anywhere
// beneath the maximal invocation. Values are derived only from the official
// declarations (representative value by declared kind; a field name selects
// the format only where the declared kind is a bare string) and are then run
// through the official package's own transform/serialization chain, so a
// witness is what an SDK caller really sends. The Rust implementation decodes
// every witness into its exact route parameter type in a unit test; a serde
// type that stops matching the SDK therefore fails `cargo test`, not release.
import assert from 'node:assert/strict';
import { pathToFileURL } from 'node:url';

import { stableJson } from './normalize.mjs';
import { MANAGED_UPLOAD_WITNESS } from './request-witnesses.mjs';

const BASE_URL = 'https://managed-request-witness.invalid';
export const HEX64_WITNESS = '0123456789abcdef'.repeat(4);
export const TIMESTAMP_WITNESS = '2026-01-01T00:00:00Z';
export const STRING_WITNESS = 'managed-witness';
export const MAP_KEY_WITNESS = 'managed_witness_key';
// Agent/Session `inference_geo` is declared as a bare string; the official
// geography vocabulary is declared beside it on Workspaces
// (`default_inference_geo?: 'global' | 'us'`). An implementation fences that
// vocabulary as a value rule, so the witness carries one declared member.
export const INFERENCE_GEO_WITNESS = 'us';
const JSON_SCHEMA_WITNESS = Object.freeze({
  type: 'object',
  properties: Object.freeze({ managed_fixture: Object.freeze({ type: 'string' }) }),
  required: Object.freeze(['managed_fixture']),
});

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
    return Object.fromEntries(
      Object.entries(value).map(([name, nested]) => [name, materialize(nested)]),
    );
  }
  return value;
}

/// A bare declared string carries no format; the official field name is the
/// only declaration-level evidence of one. Hash, timestamp, and geography
/// fields must be well formed or the implementation's value validation (not
/// its shape decoding) would refuse the witness and hide a shape regression
/// behind it.
export function representativeString(path) {
  const name = String(path.at(-1) ?? '');
  if (name.endsWith('sha256')) return HEX64_WITNESS;
  if (name.startsWith('created_at') || name.endsWith('_at')) return TIMESTAMP_WITNESS;
  if (name === 'inference_geo') return INFERENCE_GEO_WITNESS;
  return STRING_WITNESS;
}

function admittedVariants(schema, path) {
  const candidates = schema.variants.filter(({ kind }) => kind !== 'never');
  assert.ok(candidates.length > 0, `${path.join('.')}: request union contains only never`);
  return candidates;
}

function chosenVariant(schema, path) {
  const candidates = admittedVariants(schema, path);
  return candidates.find(({ kind }) => kind !== 'null') ?? candidates[0];
}

function scalarValue(schema, path) {
  switch (schema.kind) {
    case 'open-json':
      assert.equal(schema.purpose, 'json-schema', `${path.join('.')}: open request JSON purpose`);
      return JSON_SCHEMA_WITNESS;
    case 'upload': return MANAGED_UPLOAD_WITNESS;
    case 'literal': return schema.value;
    case 'string': return representativeString(path);
    case 'number': return 1;
    case 'boolean': return true;
    case 'null': return null;
    case 'never': throw new Error(`${path.join('.')}: request contract cannot materialize never`);
    default: throw new Error(`${path.join('.')}: unsupported request contract kind ${schema.kind}`);
  }
}

export function minimalValue(schema, path = []) {
  switch (schema.kind) {
    case 'object':
      return Object.fromEntries(Object.entries(schema.properties)
        .filter(([, property]) => property.required)
        .map(([name, property]) => [name, minimalValue(property.value, [...path, name])]));
    case 'union': return minimalValue(chosenVariant(schema, path), path);
    case 'array': return [];
    default: return scalarValue(schema, path);
  }
}

export function maximalValue(schema, path = []) {
  switch (schema.kind) {
    case 'object': {
      const value = Object.fromEntries(Object.entries(schema.properties)
        .map(([name, property]) => [name, maximalValue(property.value, [...path, name])]));
      if (schema.additional !== false) {
        value[MAP_KEY_WITNESS] = maximalValue(schema.additional, [...path, MAP_KEY_WITNESS]);
      }
      return value;
    }
    case 'union': return maximalValue(chosenVariant(schema, path), path);
    case 'array': return [maximalValue(schema.item, [...path, 0])];
    default: return scalarValue(schema, path);
  }
}

function variantDescriptor(variant) {
  switch (variant.kind) {
    case 'literal': return String(variant.value);
    case 'object': {
      const tag = variant.properties.type?.value;
      return tag?.kind === 'literal' ? String(tag.value) : 'object';
    }
    default: return variant.kind;
  }
}

function variantLabels(variants) {
  const descriptors = variants.map(variantDescriptor);
  return descriptors.map((descriptor, index) => (
    descriptors.indexOf(descriptor) === descriptors.lastIndexOf(descriptor)
      ? descriptor
      : `${descriptor}#${index}`
  ));
}

function withReplacement(value, at, replacement) {
  if (at.length === 0) return replacement;
  const [head, ...rest] = at;
  if (Array.isArray(value)) {
    const copy = [...value];
    copy[head] = withReplacement(value[head], rest, replacement);
    return copy;
  }
  return { ...value, [head]: withReplacement(value[head], rest, replacement) };
}

/// Every union member choice reachable beneath `maximalValue(schema)`, as a
/// replacement relative to that value: `at` addresses the union node, `label`
/// names the chosen member chain, `value` is the member's own maximal value.
/// Members nested inside an alternative member are reached too, so a shape
/// that only exists inside a non-default branch still gets one witness.
export function unionAlternatives(schema, path = []) {
  switch (schema.kind) {
    case 'union': {
      const variants = admittedVariants(schema, path);
      const chosen = chosenVariant(schema, path);
      const labels = variantLabels(variants);
      const alternatives = [];
      variants.forEach((variant, index) => {
        const nested = unionAlternatives(variant, path);
        if (variant === chosen) {
          alternatives.push(...nested);
          return;
        }
        const base = maximalValue(variant, path);
        alternatives.push({ at: [], label: labels[index], value: base });
        for (const alternative of nested) {
          alternatives.push({
            at: [],
            label: `${labels[index]}.${alternative.label}`,
            value: withReplacement(base, alternative.at, alternative.value),
          });
        }
      });
      return alternatives;
    }
    case 'object': {
      const alternatives = [];
      for (const [name, property] of Object.entries(schema.properties)) {
        for (const alternative of unionAlternatives(property.value, [...path, name])) {
          alternatives.push({
            at: [name, ...alternative.at],
            label: `${name}.${alternative.label}`,
            value: alternative.value,
          });
        }
      }
      if (schema.additional !== false) {
        const mapPath = [...path, MAP_KEY_WITNESS];
        for (const alternative of unionAlternatives(schema.additional, mapPath)) {
          alternatives.push({
            at: [MAP_KEY_WITNESS, ...alternative.at],
            label: `${MAP_KEY_WITNESS}.${alternative.label}`,
            value: alternative.value,
          });
        }
      }
      return alternatives;
    }
    case 'array':
      return unionAlternatives(schema.item, [...path, 0]).map((alternative) => ({
        at: [0, ...alternative.at],
        label: `0.${alternative.label}`,
        value: alternative.value,
      }));
    default:
      return [];
  }
}

function trimTrailingUndefined(invocation) {
  const trimmed = [...invocation];
  while (trimmed.length > 0 && trimmed.at(-1) === undefined) trimmed.pop();
  return trimmed;
}

/// The named invocations for one operation: `minimal`, `maximal`, and one per
/// union member (`<parameter>.<path>=<member>`), each as the SDK method's
/// positional argument list before materialization.
export function witnessInvocations(contract) {
  const minimal = trimTrailingUndefined(contract.parameters.map((parameter) => (
    parameter.required ? minimalValue(parameter.value, [parameter.name]) : undefined
  )));
  const maximal = contract.parameters.map((parameter) =>
    maximalValue(parameter.value, [parameter.name]));
  const invocations = [
    { id: 'minimal', invocation: minimal },
    { id: 'maximal', invocation: maximal },
  ];
  contract.parameters.forEach((parameter, index) => {
    for (const alternative of unionAlternatives(parameter.value, [parameter.name])) {
      invocations.push({
        id: `${parameter.name}.${alternative.label}`,
        invocation: withReplacement(maximal, [index, ...alternative.at], alternative.value),
      });
    }
  });
  const ids = invocations.map(({ id }) => id);
  assert.equal(new Set(ids).size, ids.length, 'witness ids are unique');
  return invocations;
}

function sortedQuery(url) {
  return [...url.searchParams.entries()].sort(([leftName, leftValue], [rightName, rightValue]) =>
    leftName.localeCompare(rightName) || leftValue.localeCompare(rightValue));
}

async function multipartParts(request) {
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

async function wireBody(request) {
  if (request.body === null) return { kind: 'empty' };
  const contentType = request.headers.get('content-type') ?? '';
  if (contentType.startsWith('multipart/form-data')) {
    return { kind: 'multipart', parts: await multipartParts(request) };
  }
  const bytes = Buffer.from(await request.arrayBuffer());
  if (bytes.length === 0) return { kind: 'empty' };
  assert.ok(
    contentType.startsWith('application/json'),
    `unexpected request content-type ${contentType}`,
  );
  return { kind: 'json', value: JSON.parse(bytes.toString('utf8')) };
}

async function wireRequest(request) {
  const url = new URL(request.url);
  const betas = request.headers.get('anthropic-beta')
    ?.split(',')
    .map((value) => value.trim())
    .filter(Boolean)
    .sort() ?? [];
  return {
    anthropic_beta: betas,
    body: await wireBody(request),
    method: request.method,
    path: decodeURIComponent(url.pathname),
    query: sortedQuery(url),
  };
}

function resourceMethod(client, operationID) {
  const parts = operationID.split('.');
  let resource = client;
  for (const part of parts.slice(0, -1)) resource = resource[part];
  return resource[parts.at(-1)].bind(resource);
}

function wireIdentity(request) {
  return JSON.stringify([request.method, request.path, request.query, request.body]);
}

/// Run every named invocation through the official package at `packageRoot`
/// and record the exact wire request. Witnesses are keyed by wire identity
/// (method, path, query, body): an invocation that only changes a transport
/// header (the `betas` / `workspace_id` parameters) collapses onto the first
/// witness with that identity, so the artifact stays a request-shape corpus
/// rather than a header matrix. An invocation the SDK refuses before
/// transport (an upstream declaration/runtime variance such as `null` inside
/// multipart form data) is recorded under `client_rejections` with its exact
/// message rather than silently dropped.
export async function officialRequestWitnesses({ packageRoot, version, operations, contracts }) {
  const { default: Anthropic } = await import(pathToFileURL(`${packageRoot}/index.mjs`));
  const captured = [];
  const fetch = async (input, init) => {
    const request = new Request(input, init);
    if (new URL(request.url).origin === BASE_URL) captured.push(await wireRequest(request.clone()));
    return new Response('{}', { status: 200, headers: { 'content-type': 'application/json' } });
  };
  const client = new Anthropic({
    apiKey: 'request-witness', // awaken-allow: secret
    baseURL: BASE_URL,
    fetch,
    maxRetries: 0,
  });
  const generated = {};
  for (const operation of operations) {
    const contract = contracts[operation.id];
    assert.ok(contract, `${operation.id}: request contract`);
    const method = resourceMethod(client, operation.id);
    const witnesses = new Map();
    const clientRejections = [];
    for (const { id, invocation } of witnessInvocations(contract)) {
      const before = captured.length;
      let response;
      try {
        response = await method(...invocation.map(materialize)).asResponse();
      } catch (error) {
        assert.equal(captured.length, before, `${operation.id} ${id}: rejected before transport`);
        clientRejections.push({ id, error: error.message });
        continue;
      }
      assert.equal(response.status, 200, `${operation.id} ${id}`);
      assert.equal(captured.length, before + 1, `${operation.id} ${id}: exactly one API request`);
      const request = captured.at(-1);
      assert.equal(request.method, operation.method, `${operation.id} ${id}: method`);
      const identity = wireIdentity(request);
      if (!witnesses.has(identity)) witnesses.set(identity, { id, ...request });
    }
    const wire = [...witnesses.values()];
    assert.equal(wire[0]?.id, 'minimal', `${operation.id}: the minimal witness reached the wire`);
    generated[operation.id] = {
      client_rejections: clientRejections,
      method: operation.method,
      path: operation.path,
      witnesses: wire,
    };
  }
  return {
    schema_version: 1,
    current_oracle: version,
    operations: generated,
  };
}

/// One witness per line: the corpus is diffed by humans and decoded by Rust,
/// so each wire request is one stable, greppable line under a pretty envelope.
export function serializedRequestWitnesses(artifact) {
  const lines = [
    '{',
    `  "current_oracle": ${JSON.stringify(artifact.current_oracle)},`,
    '  "operations": {',
  ];
  const ids = Object.keys(artifact.operations);
  ids.forEach((id, index) => {
    const operation = artifact.operations[id];
    lines.push(`    ${JSON.stringify(id)}: {`);
    lines.push(`      "client_rejections": ${JSON.stringify(stableJson(operation.client_rejections))},`);
    lines.push(`      "method": ${JSON.stringify(operation.method)},`);
    lines.push(`      "path": ${JSON.stringify(operation.path)},`);
    lines.push('      "witnesses": [');
    operation.witnesses.forEach((witness, position) => {
      const separator = position < operation.witnesses.length - 1 ? ',' : '';
      lines.push(`        ${JSON.stringify(stableJson(witness))}${separator}`);
    });
    lines.push('      ]');
    lines.push(`    }${index < ids.length - 1 ? ',' : ''}`);
  });
  lines.push('  },', `  "schema_version": ${JSON.stringify(artifact.schema_version)}`, '}');
  return `${lines.join('\n')}\n`;
}
