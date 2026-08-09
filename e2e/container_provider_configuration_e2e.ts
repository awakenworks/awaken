// Real-process fail-closed coverage for container provider composition.
//
// These are boot-time operator errors, so the observable contract is that the
// server exits before listening and names the missing build capability. This
// drives the scenario adapter with an explicitly selected Session environment;
// production resolves the equivalent tier from typed deployment configuration.

import assert from 'node:assert/strict';
import { execSync, spawn } from 'node:child_process';
import { availablePort, REPO_ROOT, pass } from './harness.mjs';

const PORT = Number(process.env.E2E_PORT ?? 38215);

function buildBrain(features: string[] = []): string {
  const args = [
    'cargo build --quiet --message-format=json -p awaken-scenario-host',
    '--bin awaken-scenario-host --no-default-features',
    ...(features.length > 0 ? [`--features ${features.join(',')}`] : []),
  ].join(' ');
  const output = execSync(args, {
    cwd: REPO_ROOT,
    env: process.env,
    maxBuffer: 128 * 1024 * 1024,
  }).toString();
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken-scenario-host') return message.executable;
    } catch {
      // Cargo may emit a non-JSON diagnostic around JSON compiler messages.
    }
  }
  throw new Error('could not resolve the scenario-host binary path');
}

async function expectBootFailure(
  binary: string,
  tier: string,
  marker: string,
  preferredPort: number,
): Promise<void> {
  // Do not derive sibling ports from one probed base: another process may own
  // any offset. The canonical harness allocator preserves the preferred port
  // when free and selects a kernel-assigned fallback when it is occupied.
  const port = await availablePort(preferredPort);
  const child = spawn(binary, {
    env: {
      ...process.env,
      AWAKEN_HTTP_ADDR: `127.0.0.1:${port}`,
      AWAKEN_MODEL_MODE: 'acp-container',
      AWAKEN_ACP_ARGV: 'node -e process.exit(0)',
      SESSION_ENVIRONMENT_TIER: tier,
      AWAKEN_CONTAINER_IMAGE: 'unused-for-missing-feature-check',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  child.stdout.on('data', (chunk) => (output += chunk));
  child.stderr.on('data', (chunk) => (output += chunk));

  const exit = await Promise.race([
    new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolve) => {
      child.once('exit', (code, signal) => resolve({ code, signal }));
    }),
    new Promise<never>((_, reject) => {
      setTimeout(() => {
        child.kill('SIGKILL');
        reject(new Error(`server did not fail closed for tier ${tier}; output=${output}`));
      }, 30_000).unref();
    }),
  ]);
  assert.notEqual(exit.code, 0, `${tier} misconfiguration must exit unsuccessfully`);
  assert.ok(output.includes(marker), `${tier} failure must name ${marker}; output=${output}`);
  assert.ok(
    !output.includes('awaken-coordinator listening on'),
    `${tier} misconfiguration must fail before accepting traffic; output=${output}`,
  );
}

async function main(): Promise<void> {
  // Cause/effect graph: C1 selected tier; C2 matching backend compiled; C3
  // preferred port free. E1 missing capability is named; E2 process exits
  // non-zero before readiness; E3 an occupied preferred port selects an
  // independent fallback and cannot masquerade as product traffic.
  //
  // Decision table:
  // R1 docker|podman|k8s + no backend feature -> generic E1+E2.
  // R2 podman|k8s + docker-only build       -> exact sibling E1+E2.
  // R3 any R1/R2 + preferred port occupied -> E3, then the same E1+E2.
  // Matching-feature success is owned by the live container scenarios; these
  // cases exclusively verify the boot-time fail-closed partition.
  // A default build has no container backend. Every configured container tier
  // uses the common no-feature failure path instead of degrading to local.
  const defaultBinary = buildBrain();
  await expectBootFailure(defaultBinary, 'docker', 'needs its matching container feature', PORT);
  await expectBootFailure(defaultBinary, 'podman', 'needs its matching container feature', PORT + 1);
  await expectBootFailure(defaultBinary, 'k8s', 'needs its matching container feature', PORT + 2);
  pass('a build without container backends fails closed for docker, podman and k8s');

  // A heterogeneous build may support one backend only. Unsupported sibling
  // tiers still fail explicitly at composition time.
  const dockerBinary = buildBrain(['container-docker']);
  await expectBootFailure(dockerBinary, 'podman', 'needs the `container-podman` feature', PORT + 3);
  await expectBootFailure(dockerBinary, 'k8s', 'needs the `container-k8s` feature', PORT + 4);
  pass('a docker-only build rejects unsupported podman and k8s tiers explicitly');

  console.log('CONTAINER PROVIDER CONFIGURATION TS API E2E PASS.');
}

main().catch((error) => {
  console.error('CONTAINER PROVIDER CONFIGURATION TS API E2E FAIL:', error);
  process.exitCode = 1;
});
