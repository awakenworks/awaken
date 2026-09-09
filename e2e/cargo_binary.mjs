// One Cargo JSON artifact resolver for every E2E that launches a Rust binary.
// Default awaken/scenario-host consumers may receive immutable snapshots from
// the deterministic runner; feature-specific consumers still build their exact
// target through the same parser and diagnostic contract.

import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { snapshotExecutable } from './executable_snapshot.mjs';

export const AWAKEN_BIN_ENV = 'AWAKEN_E2E_AWAKEN_BIN';
export const SCENARIO_HOST_BIN_ENV = 'AWAKEN_E2E_SCENARIO_HOST_BIN';
export const WORKER_BIN_ENV = 'AWAKEN_E2E_WORKER_BIN';

export function checkoutLocalCargoEnvironment(cwd, environment = process.env) {
  return {
    ...environment,
    CARGO_TARGET_DIR: environment.CARGO_TARGET_DIR || path.resolve(cwd, 'target'),
  };
}

export function parseCargoExecutable(output, targetName, targetKind) {
  for (const line of String(output).split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (
        message.reason === 'compiler-artifact'
        && typeof message.executable === 'string'
        && message.executable.length > 0
        && message.target?.name === targetName
        && message.target?.kind?.includes(targetKind)
      ) {
        return message.executable;
      }
    } catch {
      // Cargo diagnostics can contain non-JSON text; only compiler-artifact
      // records are authoritative for an executable path.
    }
  }
  throw new Error(`cargo emitted no ${targetKind} artifact for ${targetName}`);
}

export function renderedCargoDiagnostics(error) {
  const diagnostics = String(error.stdout ?? '')
    .split('\n')
    .filter(Boolean)
    .flatMap((line) => {
      try {
        const message = JSON.parse(line);
        return message.reason === 'compiler-message' && message.message?.rendered
          ? [message.message.rendered.trimEnd()]
          : [];
      } catch {
        return [];
      }
    });
  return diagnostics.join('\n') || String(error.stderr ?? '').trim();
}

export function requirePrebuiltExecutable(environmentName, environment = process.env) {
  const candidate = environment[environmentName];
  if (!candidate) return undefined;
  if (!fs.statSync(candidate, { throwIfNoEntry: false })?.isFile()) {
    throw new Error(`${environmentName} does not name a file: ${candidate}`);
  }
  return candidate;
}

/**
 * @param {{
 *   packageName: string,
 *   targetName: string,
 *   targetKind?: 'bin' | 'example',
 *   features?: string[],
 *   noDefaultFeatures?: boolean,
 *   release?: boolean,
 * }} options
 */
export function cargoArguments({
  packageName,
  targetName,
  targetKind = 'bin',
  features = [],
  noDefaultFeatures = false,
  release = false,
}) {
  const targetFlag = targetKind === 'example' ? '--example' : '--bin';
  const args = [
    'build',
    '--quiet',
    '--message-format=json',
    '-p',
    packageName,
    targetFlag,
    targetName,
  ];
  if (release) args.push('--release');
  if (noDefaultFeatures) args.push('--no-default-features');
  if (features.length > 0) args.push('--features', features.join(','));
  return args;
}

/**
 * @param {{
 *   cwd: string,
 *   packageName: string,
 *   targetName: string,
 *   targetKind?: 'bin' | 'example',
 *   features?: string[],
 *   noDefaultFeatures?: boolean,
 *   release?: boolean,
 *   environment?: NodeJS.ProcessEnv,
 *   prebuiltEnvironmentName?: string,
 *   snapshotDestination?: string,
 * }} options
 * @returns {string}
 */
export function cargoExecutable({
  cwd,
  packageName,
  targetName,
  targetKind = 'bin',
  features = [],
  noDefaultFeatures = false,
  release = false,
  environment = process.env,
  prebuiltEnvironmentName,
  snapshotDestination,
}) {
  if (prebuiltEnvironmentName) {
    const prebuilt = requirePrebuiltExecutable(prebuiltEnvironmentName, environment);
    if (prebuilt) return snapshotDestination
      ? snapshotExecutable(prebuilt, snapshotDestination) : prebuilt;
  }

  const args = cargoArguments({
    packageName,
    targetName,
    targetKind,
    features,
    noDefaultFeatures,
    release,
  });

  const buildEnvironment = checkoutLocalCargoEnvironment(cwd, environment);
  let privateOutput;
  try {
    if (snapshotDestination) {
      // Cargo owns build-directory resolution (including config templates).
      // Keep that cache and its Cargo lock, but give this invocation an
      // exclusive final-output directory. A copy AFTER a shared-target build
      // can otherwise see ENOENT or another feature set. No path inference,
      // second lock, or retry of a possibly replaced artifact is safe here.
      const metadata = JSON.parse(execFileSync('cargo', [
        'metadata', '--no-deps', '--format-version=1',
      ], { cwd, env: buildEnvironment, encoding: 'utf8', maxBuffer: 128 * 1024 * 1024 }));
      if (typeof metadata.build_directory !== 'string' || !path.isAbsolute(metadata.build_directory)) {
        throw new Error('cargo metadata did not return an absolute build_directory');
      }
      buildEnvironment.CARGO_BUILD_BUILD_DIR = metadata.build_directory;
      fs.mkdirSync(path.dirname(snapshotDestination), { recursive: true });
      privateOutput = fs.mkdtempSync(path.join(path.dirname(snapshotDestination), '.cargo-output-'));
      args.push('--target-dir', path.resolve(privateOutput));
    }
    const output = execFileSync('cargo', args, {
      cwd,
      env: buildEnvironment,
      encoding: 'utf8',
      maxBuffer: 128 * 1024 * 1024,
    });
    const executable = parseCargoExecutable(output, targetName, targetKind);
    return snapshotDestination ? snapshotExecutable(executable, snapshotDestination) : executable;
  } catch (error) {
    throw new Error(
      `cargo build failed for ${packageName}/${targetName}\n${renderedCargoDiagnostics(error) || error.message}`,
      { cause: error },
    );
  } finally {
    if (privateOutput) fs.rmSync(privateOutput, { recursive: true, force: true });
  }
}

export function scenarioHandCompanionPath(scenarioHost) {
  const suffix = process.platform === 'win32' ? '.exe' : '';
  return path.join(path.dirname(scenarioHost), `awaken-sandbox${suffix}`);
}

// Execution-host companion build decision table:
// C1 an immutable scenario-host snapshot is supplied; C2 source build is
// required (production CLI or scenario host). Effects: E1 require the release-layout sibling to exist without
// re-evaluating its capability; E2 build the existing `hand` feature and the
// scenario host into one Cargo target directory. Rules: B1 C1=>E1;
// B2 C2=>E2. The runtime remains the sole capability owner through its
// `awaken-sandbox hand --check`; this helper owns only artifact colocation.
export function cargoExecutionBundle({
  packageName,
  targetName,
  cwd,
  environment = process.env,
  prebuiltEnvironmentName,
  snapshotDirectory,
}) {
  const suffix = process.platform === 'win32' ? '.exe' : '';
  const destination = snapshotDirectory && path.join(snapshotDirectory, `${targetName}${suffix}`);
  const prebuilt = requirePrebuiltExecutable(prebuiltEnvironmentName, environment);
  if (prebuilt) {
    const handCompanion = scenarioHandCompanionPath(prebuilt);
    if (!fs.statSync(handCompanion, { throwIfNoEntry: false })?.isFile()) {
      throw new Error(
        `${prebuiltEnvironmentName} requires sibling execution companion: ${handCompanion}`,
      );
    }
    return destination ? {
      executable: snapshotExecutable(prebuilt, destination),
      handCompanion: snapshotExecutable(handCompanion, scenarioHandCompanionPath(destination)),
    } : { executable: prebuilt, handCompanion };
  }

  const handCompanion = cargoExecutable({
    cwd,
    packageName: 'awaken-sandbox',
    targetName: 'awaken-sandbox',
    features: ['hand'],
    environment,
    snapshotDestination: destination && scenarioHandCompanionPath(destination),
  });
  const executable = cargoExecutable({
    cwd,
    packageName,
    targetName,
    environment,
    snapshotDestination: destination,
  });
  const expectedCompanion = scenarioHandCompanionPath(executable);
  if (path.resolve(handCompanion) !== path.resolve(expectedCompanion)) {
    throw new Error(
      `execution host and hand companion are not siblings: ${executable}, ${handCompanion}`,
    );
  }
  return { executable, handCompanion };
}

// Scenario-specific naming is a thin adapter over the same execution bundle.
export function cargoScenarioHostBundle(options) {
  const { executable, handCompanion } = cargoExecutionBundle({
    ...options,
    packageName: 'awaken-scenario-host',
    targetName: 'awaken-scenario-host',
    prebuiltEnvironmentName: options.prebuiltEnvironmentName ?? SCENARIO_HOST_BIN_ENV,
  });
  return { scenarioHost: executable, handCompanion };
}

// Shell scenarios share the same target parser, build invocation and failure
// contract as JS E2E. Only this process adapter translates shell arguments.
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const { values, positionals } = parseArgs({
      allowPositionals: true,
      options: {
        features: { type: 'string', multiple: true },
        'no-default-features': { type: 'boolean' },
        release: { type: 'boolean' },
        'snapshot-destination': { type: 'string' },
      },
    });
    if (positionals.length !== 2) throw new Error('expected package and binary names');
    process.stdout.write(`${cargoExecutable({
      cwd: process.cwd(),
      packageName: positionals[0],
      targetName: positionals[1],
      features: values.features ?? [],
      noDefaultFeatures: values['no-default-features'] ?? false,
      release: values.release ?? false,
      snapshotDestination: values['snapshot-destination'],
    })}\n`);
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  }
}
