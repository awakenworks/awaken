// Native PG E2E preparation: reuse the existing execution-bundle owner. This
// builds before the database starts and never consumes an ambient prebuild.
import fs from 'node:fs';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import {
  AWAKEN_BIN_ENV, SCENARIO_HOST_BIN_ENV, WORKER_BIN_ENV,
  cargoExecutable, cargoScenarioHostBundle, scenarioHandCompanionPath,
} from './cargo_binary.mjs';
import { withoutAmbientProviderKeys } from './process_environment.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
export function prepareNativePostgresBundle({
  output, sourceDigest, platform = process.platform, inherited = process.env,
  fetchLocked = execFileSync, buildProduction = cargoExecutable,
  buildScenario = cargoScenarioHostBundle,
}) {
  if (platform !== 'win32' || !output
      || !path.isAbsolute(output) || !/^[a-f0-9]{64}$/.test(sourceDigest ?? '')) {
    throw new Error('native preparation requires Windows, an absolute fresh output and a source digest');
  }
  fs.mkdirSync(output); // Refuse an existing output rather than reusing it.
  const environment = withoutAmbientProviderKeys({ ...inherited });
  for (const name of [AWAKEN_BIN_ENV, SCENARIO_HOST_BIN_ENV, WORKER_BIN_ENV]) delete environment[name];
  // The existing builder resolves Cargo's private final output and preserves
  // its shared build cache. Its unmodified lock is admitted before that build.
  fetchLocked('cargo', ['fetch', '--locked', '--target', 'x86_64-pc-windows-msvc'], {
    cwd: root, env: environment, stdio: ['ignore', 'inherit', 'inherit'],
  });
  const directory = path.join(output, 'binaries');
  // Match deterministic_runner: build the production host once, then let the
  // scenario bundle own the one shared hand build and colocated snapshot.
  const production = buildProduction({ cwd: root, environment,
    packageName: 'awaken-cli', targetName: 'awaken',
    snapshotDestination: path.join(directory, 'awaken.exe') });
  const scenario = buildScenario({ cwd: root, environment, snapshotDirectory: directory });
  if (path.resolve(scenarioHandCompanionPath(production)) !== path.resolve(scenario.handCompanion)) {
    throw new Error('execution bundles must share their one colocated hand companion');
  }
  const entries = { production, scenario: scenario.scenarioHost,
    hand: scenario.handCompanion };
  const binaries = Object.fromEntries(Object.entries(entries).map(([name, executable]) =>
    [name, { path: executable, sha256: createHash('sha256').update(fs.readFileSync(executable)).digest('hex') }]));
  const record = { schema_version: 1, status: 'built', source_digest: sourceDigest, binaries };
  const pending = path.join(output, 'build.pending');
  fs.writeFileSync(pending, `${JSON.stringify(record, null, 2)}\n`);
  fs.renameSync(pending, path.join(output, 'build.json'));
  return record;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const { values } = parseArgs({ options: {
      output: { type: 'string' }, 'source-digest': { type: 'string' },
    } });
    prepareNativePostgresBundle({ output: values.output, sourceDigest: values['source-digest'] });
    console.log('Native execution bundles prepared; this is a build fact, not E2E execution.');
  } catch (error) {
    console.error('Native PG E2E preparation failed:', error.message);
    process.exitCode = Number.isInteger(error.status) && error.status > 0 ? error.status : 1;
  }
}
