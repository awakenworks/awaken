import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';

const require = createRequire(import.meta.url);
const packageRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const anchorsPath = path.join(packageRoot, 'config/anchors.json');

export function readSdkMatrix() {
  const matrix = JSON.parse(fs.readFileSync(anchorsPath, 'utf8'));
  assert.equal(matrix.schema_version, 1, 'unsupported SDK anchor schema');
  assert.ok(Array.isArray(matrix.anchors), 'SDK anchors must be an array');
  return matrix.anchors;
}

export function installedPackage(moduleName) {
  let current = path.dirname(require.resolve(moduleName));
  for (;;) {
    const manifestPath = path.join(current, 'package.json');
    if (fs.existsSync(manifestPath)) {
      const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
      if (manifest.name === '@anthropic-ai/sdk') {
        return { root: current, version: manifest.version };
      }
    }
    const parent = path.dirname(current);
    if (parent === current) {
      throw new Error(`cannot resolve SDK package metadata for ${moduleName}`);
    }
    current = parent;
  }
}

export function installedPackageVersion(moduleName) {
  return installedPackage(moduleName).version;
}

export function validateSdkMatrix(entries) {
  const ids = new Set();
  const modules = new Set();
  const roles = new Set();
  for (const entry of entries) {
    assert.ok(entry.id && entry.module && entry.role, 'every SDK anchor requires id, module, and role');
    assert.ok(!ids.has(entry.id), `duplicate SDK anchor id ${entry.id}`);
    assert.ok(!modules.has(entry.module), `duplicate SDK module ${entry.module}`);
    assert.ok(!roles.has(entry.role), `duplicate SDK role ${entry.role}`);
    ids.add(entry.id);
    modules.add(entry.module);
    roles.add(entry.role);
  }
  for (const role of [
    'oldest_supported',
    'protocol_change_point',
    'beta_resource_projection_change_point',
    'current_oracle',
  ]) {
    assert.ok(roles.has(role), `SDK matrix is missing ${role}`);
  }
  assert.equal(roles.size, 4, 'SDK matrix contains an unreviewed parallel role');
  return entries;
}

export async function loadQualifiedClients(entries = readSdkMatrix()) {
  validateSdkMatrix(entries);
  return Promise.all(entries.map(async (entry) => {
    const sdk = await import(entry.module);
    const installed = installedPackage(entry.module);
    return {
      ...entry,
      ...installed,
      Client: sdk.default,
      toFile: sdk.toFile,
    };
  }));
}

export async function loadConformanceClients(
  entries = readSdkMatrix(),
  candidateModule = process.env.ANTHROPIC_SDK_CONFORMANCE_CANDIDATE,
  candidateVersion = process.env.ANTHROPIC_SDK_CONFORMANCE_CANDIDATE_VERSION,
) {
  const clients = await loadQualifiedClients(entries);
  if (!candidateModule && !candidateVersion) return clients;
  assert.equal(
    candidateModule,
    '@anthropic-ai/sdk-candidate',
    'the conformance candidate must use the reviewed exact package alias',
  );
  assert.ok(
    !entries.some(({ module }) => module === candidateModule),
    'the candidate cannot duplicate a supported SDK anchor',
  );
  assert.match(
    candidateVersion ?? '',
    /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/u,
    'the conformance candidate requires one exact reviewed version',
  );
  const sdk = await import(candidateModule);
  const installed = installedPackage(candidateModule);
  assert.equal(
    installed.version,
    candidateVersion,
    'the installed conformance candidate must match its reviewed version',
  );
  return [...clients, {
    id: 'candidate',
    module: candidateModule,
    role: 'candidate',
    ...installed,
    Client: sdk.default,
    toFile: sdk.toFile,
  }];
}

export function currentAndCandidateClients(clients) {
  const current = qualifiedClient(clients, 'current_oracle');
  const candidates = clients.filter(({ role }) => role === 'candidate');
  assert.ok(candidates.length <= 1, 'expected at most one reviewed candidate SDK client');
  return [current, ...candidates];
}

export function qualifiedClient(clients, role) {
  const matches = clients.filter((client) => client.role === role);
  assert.equal(matches.length, 1, `expected exactly one ${role} SDK client`);
  return matches[0];
}
