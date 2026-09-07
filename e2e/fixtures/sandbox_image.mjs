import { execFileSync, spawnSync, spawn } from 'node:child_process';

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
  if (existing.status === 0 && existing.stdout.trim() === '2') return;
  execFileSync('bash', ['deploy/images/sandbox/build.sh', image, ''], {
    cwd: repoRoot,
    env: { ...process.env, CONTAINER_ENGINE: engine },
    stdio: 'inherit',
    timeout: Math.max(timeoutMs, 900_000),
  });
}

/** Fail before model/approval waits if the selected substrate cannot reach its
 * local fixture. This is network admission evidence, never a model response or
 * a substitute for the actual Session's capability and inference checks. */
export async function assertContainerFixtureReachable({engine, image, url, kubeconfig, namespace}) {
  if (!['docker', 'podman', 'k8s'].includes(engine)) throw new Error('Unsupported fixture substrate');
  if (engine === 'k8s' && (!kubeconfig || !namespace)) throw new Error('Kubernetes fixture requires explicit kubeconfig and namespace');
  const endpoint = new URL(url);
  if (!['http:', 'https:'].includes(endpoint.protocol) || endpoint.username || endpoint.password) {
    throw new Error('Fixture endpoint must be credential-free HTTP(S)');
  }
  // HEAD observes connectivity without opening an MCP GET/SSE stream.
  const args = ['--head', '--noproxy', '*', '--max-time', '5', '--silent', '--show-error',
    '--output', '/dev/null', '--write-out', 'AWAKEN_FIXTURE_HTTP=%{http_code}', endpoint.href];
  const name = `cowork-route-${process.pid}-${Date.now()}`;
  const command = engine === 'k8s' ? 'kubectl' : engine;
  const argv = engine === 'k8s'
    ? ['--kubeconfig', kubeconfig, '-n', namespace, 'run', name, '--rm', '-i',
       '--restart=Never', '--labels=app=awaken-sandbox,awaken-egress=open', '--image', image, '--pod-running-timeout=60s', '--command', '--', 'curl', ...args]
    : ['run', '--rm', '--name', name, '--entrypoint', 'curl', image, ...args];
  const child = spawn(command, argv, {stdio:['ignore','pipe','pipe'], signal:AbortSignal.timeout(engine === 'k8s' ? 90_000 : 15_000)});
  let output = '';
  child.stdout.on('data', data => { output += data; });
  // Do not include command arguments or external server bodies in errors.
  child.stderr.resume();
  let status;
  try {
    status = await new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('close', resolve);
    });
  } finally {
    // The exact helper object belongs to this invocation; even interrupted
    // kubectl --rm must not leave a diagnostic Pod behind. No Session is touched.
    const cleanup = engine === 'k8s'
      ? ['--kubeconfig', kubeconfig, '-n', namespace, 'delete', 'pod', name, '--ignore-not-found', '--wait=false']
      : ['rm', '-f', name];
    spawnSync(command, cleanup, {stdio:'ignore',timeout:10_000});
  }
  if (status !== 0 || !/AWAKEN_FIXTURE_HTTP=[1-5][0-9]{2}(?![0-9])/.test(output)) {
    throw new Error(`${engine} cannot reach fixture ${endpoint.origin}; network preflight failed before fault injection (exit ${status})`);
  }
}
