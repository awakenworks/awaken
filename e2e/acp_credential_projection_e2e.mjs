// Native ACP credential projection startup gate, against the real aggregated
// `awaken` process built with the Docker sandbox adapter. The credential remains a
// host-side file handled by the sandbox SecretBroker; the sandbox configuration
// receives only a credential reference. These cases prove the composition root
// rejects ambiguous or unsafe injection before serving traffic.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execSync, spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-credential-e2e-'));

function awakenBin() {
  const output = execSync(
    'cargo build --quiet --message-format=json -p awaken-cli --bin awaken --features container-docker',
    { cwd: ROOT, maxBuffer: 128 * 1024 * 1024 },
  ).toString();
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken') return message.executable;
    } catch {
      // Cargo may emit a non-JSON diagnostic around structured messages.
    }
  }
  throw new Error('could not resolve the container-enabled awaken binary');
}

function runToExit(binary, environment, timeoutMs = 20_000) {
  const inherited = { ...process.env };
  for (const key of [
    'AWAKEN_ACP_ARGV',
    'AWAKEN_ACP_CLI',
    'AWAKEN_ACP_CREDENTIAL_FILE',
    'AWAKEN_CONTAINER_IMAGE',
  ]) delete inherited[key];
  return new Promise((resolve) => {
    const child = spawn(binary, {
      env: {
        ...inherited,
        AWAKEN_ROLE: 'serve',
        AWAKEN_MODEL_MODE: 'echo',
        AWAKEN_SANDBOX_TIER: 'docker',
        AWAKEN_SANDBOX_REAP: '0',
        AWAKEN_CONTAINER_IMAGE: 'unused-e2e-image',
        ...environment,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let output = '';
    child.stdout.on('data', (chunk) => (output += chunk.toString()));
    child.stderr.on('data', (chunk) => (output += chunk.toString()));
    const timer = setTimeout(() => child.kill('SIGKILL'), timeoutMs);
    child.once('exit', (code, signal) => {
      clearTimeout(timer);
      resolve({ code, signal, output });
    });
  });
}

async function rejects(binary, environment, expected) {
  const result = await runToExit(binary, environment);
  assert.notEqual(result.code, 0, `unsafe credential projection served traffic: ${result.output}`);
  assert.equal(result.signal, null, `process timed out instead of rejecting configuration: ${result.output}`);
  assert.ok(result.output.includes(expected), `expected ${JSON.stringify(expected)} in: ${result.output}`);
}

async function main() {
  const binary = awakenBin();
  const secure = path.join(TMP, 'secure.json');
  const insecure = path.join(TMP, 'insecure.json');
  const directory = path.join(TMP, 'directory');
  fs.writeFileSync(secure, '{"token":"host-only"}', { mode: 0o600 }); // awaken-allow: secret (fixture)
  fs.writeFileSync(insecure, '{"token":"too-open"}', { mode: 0o644 }); // awaken-allow: secret (fixture)
  fs.mkdirSync(directory);

  await rejects(binary, {
    AWAKEN_ACP_ARGV: 'unused --acp',
    AWAKEN_ACP_CREDENTIAL_FILE: secure,
  }, 'requires a projected AWAKEN_ACP_CLI');

  await rejects(binary, {
    AWAKEN_ACP_CLI: 'gemini',
    AWAKEN_ACP_CREDENTIAL_FILE: secure,
  }, 'has no native credential file');

  await rejects(binary, {
    AWAKEN_ACP_CLI: 'codex',
    AWAKEN_ACP_CREDENTIAL_FILE: path.join(TMP, 'missing.json'),
  }, 'ACP credential file is not readable');

  await rejects(binary, {
    AWAKEN_ACP_CLI: 'codex',
    AWAKEN_ACP_CREDENTIAL_FILE: directory,
  }, 'ACP credential path must name a regular file');

  await rejects(binary, {
    AWAKEN_ACP_CLI: 'codex',
    AWAKEN_ACP_CREDENTIAL_FILE: insecure,
  }, 'ACP credential file must be owner-only (mode 0600)');

  // A valid owner-only file passes credential projection. The next independent
  // deployment guard (missing image) then fails, proving no credential bytes were
  // needed in IAM, policy, Session, or resource persistence to compose the broker.
  await rejects(binary, {
    AWAKEN_ACP_CLI: 'codex',
    AWAKEN_ACP_CREDENTIAL_FILE: secure,
    AWAKEN_CONTAINER_IMAGE: '',
  }, 'a container sandbox tier requires AWAKEN_CONTAINER_IMAGE');

  console.log('E2E PASS: ACP credential injection rejects fixed, unsupported, missing, non-file, and insecure sources; a valid host-only file reaches the container guard.');
}

try {
  await main();
} finally {
  fs.rmSync(TMP, { recursive: true, force: true });
}
