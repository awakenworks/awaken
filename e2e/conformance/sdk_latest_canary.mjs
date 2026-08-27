import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { resolveSdkPackage } from '../../packages/managed-sdk-oracle/src/package-source.mjs';
import { latestCanaryPlan } from './sdk_latest_canary_lib.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const E2E = resolve(HERE, '..');
const REPO = resolve(E2E, '..');
const oracle = JSON.parse(readFileSync(
  resolve(REPO, 'contracts/anthropic-managed/upstream-oracle.generated.json'),
  'utf8',
));
const installed = resolveSdkPackage(oracle.current.module);
const latest = JSON.parse(execFileSync(
  'npm', ['view', '@anthropic-ai/sdk', 'version', '--json'], { encoding: 'utf8' },
));
const plan = latestCanaryPlan(oracle.current.version, latest, installed.version);

execFileSync('pnpm', ['--filter', '@awaken/managed-sdk-oracle', 'check'], {
  cwd: REPO,
  stdio: 'inherit',
});
execFileSync(process.execPath, [resolve(HERE, 'sdk_surface_coverage_e2e.mjs')], {
  cwd: E2E,
  stdio: 'inherit',
});
execFileSync(process.execPath, [resolve(HERE, 'sdk_latest_runtime_canary.mjs')], {
  cwd: E2E,
  env: { ...process.env, ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT: installed.root },
  stdio: 'inherit',
});
console.log(
  `SDK LATEST CANARY PASS: oracle=${plan.oracle}, registry=${plan.latest}; `
  + 'the generated Managed SDK anchor owns declaration and runtime evidence.',
);
