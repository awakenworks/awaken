// One Cargo JSON artifact resolver for every E2E that launches a Rust binary.
// Default awaken/scenario-host consumers may receive immutable snapshots from
// the deterministic runner; feature-specific consumers still build their exact
// target through the same parser and diagnostic contract.

import fs from 'node:fs';
import path from 'node:path';
import { execFileSync } from 'node:child_process';

export const AWAKEN_BIN_ENV = 'AWAKEN_E2E_AWAKEN_BIN';
export const SCENARIO_HOST_BIN_ENV = 'AWAKEN_E2E_SCENARIO_HOST_BIN';
export const WORKER_BIN_ENV = 'AWAKEN_E2E_WORKER_BIN';

export function parseCargoExecutable(output, targetName, targetKind) {
  for (const line of String(output).split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (
        message.executable
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
 * }} options
 */
export function cargoArguments({
  packageName,
  targetName,
  targetKind = 'bin',
  features = [],
  noDefaultFeatures = false,
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
 *   environment?: NodeJS.ProcessEnv,
 *   prebuiltEnvironmentName?: string,
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
  environment = process.env,
  prebuiltEnvironmentName,
}) {
  if (prebuiltEnvironmentName) {
    const prebuilt = requirePrebuiltExecutable(prebuiltEnvironmentName, environment);
    if (prebuilt) return prebuilt;
  }

  const args = cargoArguments({
    packageName,
    targetName,
    targetKind,
    features,
    noDefaultFeatures,
  });

  let output;
  try {
    output = execFileSync('cargo', args, {
      cwd,
      env: environment,
      encoding: 'utf8',
      maxBuffer: 128 * 1024 * 1024,
    });
  } catch (error) {
    throw new Error(
      `cargo build failed for ${packageName}/${targetName}\n${renderedCargoDiagnostics(error)}`,
      { cause: error },
    );
  }
  return parseCargoExecutable(output, targetName, targetKind);
}

export function scenarioHandCompanionPath(scenarioHost) {
  const suffix = process.platform === 'win32' ? '.exe' : '';
  return path.join(path.dirname(scenarioHost), `awaken-sandbox${suffix}`);
}

// Scenario-host companion build decision table:
// C1 an immutable scenario-host snapshot is supplied; C2 source build is
// required. Effects: E1 require the release-layout sibling to exist without
// re-evaluating its capability; E2 build the existing `hand` feature and the
// scenario host into one Cargo target directory. Rules: B1 C1=>E1;
// B2 C2=>E2. The runtime remains the sole capability owner through its
// `awaken-sandbox hand --check`; this helper owns only artifact colocation.
export function cargoScenarioHostBundle({
  cwd,
  environment = process.env,
  prebuiltEnvironmentName = SCENARIO_HOST_BIN_ENV,
}) {
  const prebuilt = requirePrebuiltExecutable(prebuiltEnvironmentName, environment);
  if (prebuilt) {
    const handCompanion = scenarioHandCompanionPath(prebuilt);
    if (!fs.statSync(handCompanion, { throwIfNoEntry: false })?.isFile()) {
      throw new Error(
        `${prebuiltEnvironmentName} requires sibling scenario companion: ${handCompanion}`,
      );
    }
    return { scenarioHost: prebuilt, handCompanion };
  }

  const handCompanion = cargoExecutable({
    cwd,
    packageName: 'awaken-sandbox',
    targetName: 'awaken-sandbox',
    features: ['hand'],
    environment,
  });
  const scenarioHost = cargoExecutable({
    cwd,
    packageName: 'awaken-scenario-host',
    targetName: 'awaken-scenario-host',
    environment,
  });
  const expectedCompanion = scenarioHandCompanionPath(scenarioHost);
  if (path.resolve(handCompanion) !== path.resolve(expectedCompanion)) {
    throw new Error(
      `scenario host and hand companion are not siblings: ${scenarioHost}, ${handCompanion}`,
    );
  }
  return { scenarioHost, handCompanion };
}
