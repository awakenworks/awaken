import { execFileSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { latestCanaryPlan } from './sdk_latest_canary_lib.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const E2E = resolve(HERE, '..');
const manifest = JSON.parse(readFileSync(resolve(E2E, 'package.json'), 'utf8'));
const pinned = manifest.dependencies['@anthropic-ai/sdk'];
const latest = JSON.parse(execFileSync(
  'npm', ['view', '@anthropic-ai/sdk', 'version', '--json'], { encoding: 'utf8' },
));
const plan = latestCanaryPlan(pinned, latest);
let temporary;

try {
  let packageRoot = resolve(E2E, 'node_modules', '@anthropic-ai', 'sdk');
  if (plan.fetchLatest) {
    temporary = mkdtempSync(resolve(tmpdir(), 'awaken-anthropic-sdk-canary-'));
    const packed = JSON.parse(execFileSync(
      'npm', ['pack', `@anthropic-ai/sdk@${latest}`, '--pack-destination', temporary, '--json'],
      { encoding: 'utf8' },
    ));
    if (!Array.isArray(packed) || packed.length !== 1 || typeof packed[0].filename !== 'string') {
      throw new Error(`npm pack returned an unexpected manifest: ${JSON.stringify(packed)}`);
    }
    execFileSync('tar', ['-xzf', resolve(temporary, packed[0].filename), '-C', temporary]);
    packageRoot = resolve(temporary, 'package');
  }

  execFileSync(process.execPath, [resolve(HERE, 'sdk_surface_coverage_e2e.mjs')], {
    cwd: E2E,
    env: { ...process.env, ANTHROPIC_SDK_PACKAGE_ROOT: packageRoot },
    stdio: 'inherit',
  });
  let runtimePackageRoot = packageRoot;
  if (plan.fetchLatest) {
    const runtime = resolve(temporary, 'runtime');
    mkdirSync(runtime);
    execFileSync(
      'npm',
      [
        'install', '--ignore-scripts', '--no-audit', '--no-fund', '--prefix', runtime,
        `@anthropic-ai/sdk@${latest}`,
      ],
      { stdio: 'inherit' },
    );
    runtimePackageRoot = resolve(runtime, 'node_modules', '@anthropic-ai', 'sdk');
  }
  execFileSync(process.execPath, [resolve(HERE, 'sdk_latest_runtime_canary.mjs')], {
    cwd: E2E,
    env: { ...process.env, ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT: runtimePackageRoot },
    stdio: 'inherit',
  });
  console.log(`SDK LATEST CANARY PASS: pinned=${pinned}, registry=${latest}; Managed declarations and runtime behavior are reviewed-compatible.`);
} finally {
  if (temporary) rmSync(temporary, { recursive: true, force: true });
}
