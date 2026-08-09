// Managed Environment package image publication and cross-engine-cache reuse.
//
// A local OCI registry receives the content-addressed image. The first managed
// run builds and pushes it; the local derived image is then removed, and a
// second full SDK -> Session -> container run must pull the immutable digest.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';

const docker = (...args) => spawnSync('docker', args, { encoding: 'utf8', timeout: 60_000 });
const required = process.env.AWAKEN_E2E_REQUIRE_CONTAINER === '1';
const probe = docker('info', '--format', '{{.ServerVersion}}');
if (probe.status !== 0) {
  if (required) throw new Error(`Docker is required: ${probe.stderr}`);
  console.log('SKIP: Docker is unavailable');
  process.exit(0);
}

const port = Number(process.env.AWAKEN_E2E_REGISTRY_PORT ?? 38991);
const registry = `127.0.0.1:${port}`;
const name = `awaken-package-registry-${process.pid}`;
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-package-registry-'));
const htpasswd = path.join(root, 'htpasswd');
const authFile = path.join(root, 'auth.json');
const basic = Buffer.from('awaken:test-secret').toString('base64');
fs.writeFileSync(
  htpasswd,
  'awaken:$2y$05$rjG/u5X57lMosiIQS1.wsuDEBt9Dpq9FJ/.IPxTxnnutSBX.x4Fi.\n',
  { mode: 0o600 },
);
fs.writeFileSync(
  authFile,
  JSON.stringify({ auths: { [registry]: { auth: basic } } }),
  { mode: 0o600 },
);

function run(args, message) {
  const result = docker(...args);
  assert.equal(result.status, 0, `${message}: ${result.stderr || result.stdout}`);
  return result.stdout.trim();
}

function managedRun() {
  const result = spawnSync('node', ['managed_container_agent_e2e.mjs'], {
    cwd: import.meta.dirname,
    encoding: 'utf8',
    // A cold Rust build on a constrained CI worker can take several minutes;
    // the nested managed run itself remains bounded.
    timeout: 1_200_000,
    env: {
      ...process.env,
      AWAKEN_PACKAGE_IMAGE_REGISTRY: registry,
      AWAKEN_PACKAGE_REGISTRY_AUTH_FILE: authFile,
      AWAKEN_E2E_PACKAGE_ONLY: '1',
      AWAKEN_E2E_PACKAGE_REGISTRY_ONLY: '1',
      AWAKEN_E2E_REQUIRE_CONTAINER: '1',
      AWAKEN_E2E_CONTAINER_ENGINE: 'docker',
    },
  });
  assert.equal(
    result.status,
    0,
    result.error?.message ?? result.stderr ?? result.stdout,
  );
}

try {
  run(
    [
      'run', '-d', '--name', name,
      '-p', `127.0.0.1:${port}:5000`,
      '-v', `${htpasswd}:/auth/htpasswd:ro`,
      '-e', 'REGISTRY_AUTH=htpasswd',
      '-e', 'REGISTRY_AUTH_HTPASSWD_REALM=Awaken package registry',
      '-e', 'REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd',
      'registry:2',
    ],
    'start local OCI registry',
  );
  managedRun();

  const catalog = await fetch(`http://${registry}/v2/_catalog`, {
    headers: { authorization: `Basic ${basic}` },
  }).then((response) => {
    assert.equal(response.status, 200);
    return response.json();
  });
  assert.ok(catalog.repositories.includes('awaken-packages'));

  const tags = run(
    ['images', '--format', '{{.Repository}}:{{.Tag}}', `${registry}/awaken-packages`],
    'list local package images',
  ).split(/\s+/).filter(Boolean);
  assert.ok(tags.length > 0, 'the first run must leave a local builder cache');
  for (const tag of tags) run(['image', 'rm', tag], `remove local cache ${tag}`);

  managedRun();
  console.log(
    'E2E PASS: package image was pushed by digest and reused from OCI Registry after local cache removal.',
  );
} finally {
  docker('stop', name);
  docker('rm', name);
  fs.rmSync(root, { recursive: true, force: true });
}
