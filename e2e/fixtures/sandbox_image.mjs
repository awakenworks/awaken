import { execFileSync, spawnSync } from 'node:child_process';

/** Build/reuse the one canonical production sandbox image used by container E2Es. */
export function ensureCanonicalSandboxImage({
  engine,
  image,
  repoRoot,
  timeoutMs = 30_000,
}) {
  const existing = spawnSync(engine, [
    'image', 'inspect', '--format',
    '{{index .Config.Labels "org.awaken.environment-packages"}}', image,
  ], { encoding: 'utf8', timeout: timeoutMs });
  if (existing.status === 0 && existing.stdout.trim() === '1') return;
  execFileSync('bash', ['deploy/images/sandbox/build.sh', image, ''], {
    cwd: repoRoot,
    env: { ...process.env, CONTAINER_ENGINE: engine },
    stdio: 'inherit',
    timeout: Math.max(timeoutMs, 900_000),
  });
}
