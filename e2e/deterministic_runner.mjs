// Canonical deterministic E2E executor. Suite membership stays in package.json;
// this file expands it, snapshots the common Rust binaries once, records
// per-command timings, and optionally selects one duration-balanced CI shard.
//
//   npm run test:deterministic -- --shard 1/4
//   AWAKEN_E2E_TIMINGS_INPUT=target/test-timings/e2e-1-of-1.json \
//     npm run test:deterministic -- --shard 1/4
//   AWAKEN_E2E_PREBUILT_DIR=target/e2e-prebuilt \
//     npm run test:deterministic -- --prebuild-only

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createHash } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import {
  AWAKEN_BIN_ENV,
  SCENARIO_HOST_BIN_ENV,
  WORKER_BIN_ENV,
  cargoExecutable,
  requirePrebuiltExecutable,
} from './cargo_binary.mjs';

const E2E_ROOT = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(E2E_ROOT, '..');

export function splitCommands(script) {
  return script.split(/\s*&&\s*/).filter(Boolean);
}

export function expandSuites(scripts, suiteNames) {
  const expanded = [];
  const visit = (name, stack = []) => {
    if (stack.includes(name)) {
      throw new Error(`cyclic npm suite: ${[...stack, name].join(' -> ')}`);
    }
    if (!(name in scripts)) throw new Error(`unknown npm suite: ${name}`);
    if (name === 'test' && scripts.pretest) visit('pretest', [...stack, name]);
    for (const command of splitCommands(scripts[name])) {
      const nested = command.match(/^npm run ([^ ]+)$/);
      if (nested && scripts[nested[1]]) visit(nested[1], [...stack, name]);
      else expanded.push({ command, owner: [...stack, name].join(' > ') });
    }
  };
  for (const name of suiteNames) visit(name);
  return expanded;
}

export function assertCanonicalPortOwnership(commands) {
  const override = commands.find(({ command }) => /(?:^|\s)E2E_(?:WORKER_)?PORT=/u.test(command));
  if (override) {
    throw new Error(
      `deterministic command overrides harness-owned port allocation: ${override.command}`,
    );
  }
}

export function parseShard(value) {
  if (!value) return { index: 0, total: 1 };
  const match = value.match(/^(\d+)\/(\d+)$/);
  if (!match) throw new Error(`invalid shard ${value}; expected INDEX/TOTAL`);
  const index = Number(match[1]);
  const total = Number(match[2]);
  if (total < 1 || index < 1 || index > total) {
    throw new Error(`invalid shard ${value}; INDEX is one-based and must not exceed TOTAL`);
  }
  return { index: index - 1, total };
}

export function assignShard(commands, shard, historicalDurations = new Map()) {
  if (shard.total === 1) return commands;
  const buckets = Array.from({ length: shard.total }, () => ({ weight: 0, commands: [] }));
  const weighted = commands
    .map((entry, order) => ({
      ...entry,
      order,
      weight: Math.max(1, historicalDurations.get(entry.command) ?? 1),
    }))
    .sort((left, right) => right.weight - left.weight || left.order - right.order);
  for (const entry of weighted) {
    const bucket = buckets.reduce((lightest, candidate) => (
      candidate.weight < lightest.weight ? candidate : lightest
    ));
    bucket.commands.push(entry);
    bucket.weight += entry.weight;
  }
  return buckets[shard.index].commands.sort((a, b) => a.order - b.order);
}

export function timingWeights(document) {
  const weights = new Map();
  for (const entry of document?.commands ?? []) {
    if (typeof entry.command === 'string' && Number.isFinite(entry.durationMs)) {
      weights.set(entry.command, Math.max(weights.get(entry.command) ?? 0, entry.durationMs));
    }
  }
  return weights;
}

export function prebuildFingerprint(environment = process.env) {
  const hash = createHash('sha256');
  const files = execFileSync(
    'git',
    ['ls-files', '-z', '--cached', '--others', '--exclude-standard'],
    { cwd: REPO_ROOT, maxBuffer: 64 * 1024 * 1024 },
  ).toString().split('\0').filter(Boolean).sort();
  for (const relative of files) {
    const source = path.join(REPO_ROOT, relative);
    if (!fs.statSync(source, { throwIfNoEntry: false })?.isFile()) continue;
    hash.update(relative).update('\0').update(fs.readFileSync(source)).update('\0');
  }
  hash.update(execFileSync('rustc', ['-Vv'], { encoding: 'utf8' }));
  hash.update(execFileSync('cargo', ['-V'], { encoding: 'utf8' }));
  hash.update(`${process.platform}\0${process.arch}\0`);
  const exactBuildEnvironmentKeys = new Set([
    'CARGO_BUILD_RUSTFLAGS',
    'CARGO_BUILD_TARGET',
    'CARGO_ENCODED_RUSTFLAGS',
    'RUSTC',
    'RUSTC_WORKSPACE_WRAPPER',
    'RUSTC_WRAPPER',
    'RUSTDOCFLAGS',
    'RUSTFLAGS',
    'SOURCE_DATE_EPOCH',
  ]);
  for (const [key, value] of Object.entries(environment).sort(([left], [right]) => (
    left.localeCompare(right)
  ))) {
    if (exactBuildEnvironmentKeys.has(key) || key.startsWith('CARGO_PROFILE_')) {
      hash.update(`${key}=${value}\0`);
    }
  }
  return `sha256:${hash.digest('hex')}`;
}

export function fileDigest(file) {
  const hash = createHash('sha256');
  const descriptor = fs.openSync(file, 'r');
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  try {
    let bytesRead;
    while ((bytesRead = fs.readSync(descriptor, buffer, 0, buffer.length, null)) > 0) {
      hash.update(buffer.subarray(0, bytesRead));
    }
  } finally {
    fs.closeSync(descriptor);
  }
  return `sha256:${hash.digest('hex')}`;
}

// Keep the canonical prebuilt trio under the repository target by default.
// Namespace E2Es bind that one generated tree writable while keeping the
// checkout and the rest of the host read-only; a host `/tmp` cache would be
// hidden by their private tmpfs and would force a forbidden in-sandbox build.
export function prebuiltDirectory(explicitDirectory) {
  return explicitDirectory ?? 'target/e2e-prebuilt';
}

export function validPrebuiltManifest(
  manifest,
  fingerprint,
  dependencyLockDigest,
  awaken,
  scenarioHost,
  worker,
) {
  return manifest?.version === 2
    && manifest.fingerprint === fingerprint
    && manifest.dependencyLockDigest === dependencyLockDigest
    && manifest.binaries?.awaken === fileDigest(awaken)
    && manifest.binaries?.scenarioHost === fileDigest(scenarioHost)
    && manifest.binaries?.worker === fileDigest(worker);
}

function snapshotExecutable(source, destination) {
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  fs.copyFileSync(source, destination);
  fs.chmodSync(destination, 0o755);
  return destination;
}

export function preparedEnvironment(environment, explicitDirectory) {
  const inheritedAwaken = requirePrebuiltExecutable(AWAKEN_BIN_ENV, environment);
  const inheritedScenarioHost = requirePrebuiltExecutable(SCENARIO_HOST_BIN_ENV, environment);
  const inheritedWorker = requirePrebuiltExecutable(WORKER_BIN_ENV, environment);
  if (inheritedAwaken && inheritedScenarioHost && inheritedWorker) return { ...environment };
  if (inheritedAwaken || inheritedScenarioHost || inheritedWorker) {
    throw new Error(
      `${AWAKEN_BIN_ENV}, ${SCENARIO_HOST_BIN_ENV}, and ${WORKER_BIN_ENV} must be supplied together`,
    );
  }

  const suffix = process.platform === 'win32' ? '.exe' : '';
  const directory = explicitDirectory
    ? path.resolve(REPO_ROOT, explicitDirectory)
    : fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-e2e-prebuilt-'));
  const awakenDestination = path.join(directory, `awaken${suffix}`);
  const scenarioHostDestination = path.join(directory, `awaken-scenario-host${suffix}`);
  const workerDestination = path.join(directory, `awaken-worker${suffix}`);
  const manifestPath = path.join(directory, 'manifest.json');
  const fingerprint = prebuildFingerprint(environment);
  const dependencyLockDigest = fileDigest(path.join(E2E_ROOT, 'package-lock.json'));
  const existingAwaken = fs.statSync(awakenDestination, { throwIfNoEntry: false })?.isFile();
  const existingScenarioHost = fs.statSync(
    scenarioHostDestination,
    { throwIfNoEntry: false },
  )?.isFile();
  const existingWorker = fs.statSync(workerDestination, { throwIfNoEntry: false })?.isFile();
  if (existingAwaken || existingScenarioHost || existingWorker) {
    if (!existingAwaken || !existingScenarioHost || !existingWorker) {
      throw new Error(`incomplete E2E prebuilt directory: ${directory}`);
    }
    let manifest;
    try {
      manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
    } catch {
      manifest = undefined;
    }
    if (validPrebuiltManifest(
      manifest,
      fingerprint,
      dependencyLockDigest,
      awakenDestination,
      scenarioHostDestination,
      workerDestination,
    )) {
      return {
        ...environment,
        [AWAKEN_BIN_ENV]: awakenDestination,
        [SCENARIO_HOST_BIN_ENV]: scenarioHostDestination,
        [WORKER_BIN_ENV]: workerDestination,
      };
    }
    fs.rmSync(awakenDestination, { force: true });
    fs.rmSync(scenarioHostDestination, { force: true });
    fs.rmSync(workerDestination, { force: true });
    fs.rmSync(manifestPath, { force: true });
  }
  if (!explicitDirectory) {
    process.on('exit', () => fs.rmSync(directory, { recursive: true, force: true }));
  }
  const awaken = cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-cli',
    targetName: 'awaken',
  });
  const scenarioHost = cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-scenario-host',
    targetName: 'awaken-scenario-host',
  });
  const worker = cargoExecutable({
    cwd: REPO_ROOT,
    packageName: 'awaken-worker',
    targetName: 'awaken-worker',
  });
  const prepared = {
    ...environment,
    [AWAKEN_BIN_ENV]: snapshotExecutable(awaken, awakenDestination),
    [SCENARIO_HOST_BIN_ENV]: snapshotExecutable(
      scenarioHost,
      scenarioHostDestination,
    ),
    [WORKER_BIN_ENV]: snapshotExecutable(worker, workerDestination),
  };
  fs.mkdirSync(directory, { recursive: true });
  const temporaryManifest = `${manifestPath}.${process.pid}.tmp`;
  fs.writeFileSync(
    temporaryManifest,
    `${JSON.stringify({
      version: 2,
      fingerprint,
      dependencyLockDigest,
      binaries: {
        awaken: fileDigest(awakenDestination),
        scenarioHost: fileDigest(scenarioHostDestination),
        worker: fileDigest(workerDestination),
      },
    }, null, 2)}\n`,
  );
  fs.renameSync(temporaryManifest, manifestPath);
  return prepared;
}

function writeTimings(file, report) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const temporary = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(temporary, `${JSON.stringify(report, null, 2)}\n`);
  fs.renameSync(temporary, file);
}

function loadPackage() {
  return JSON.parse(fs.readFileSync(path.join(E2E_ROOT, 'package.json'), 'utf8'));
}

export function verifyInstalledDependencies(
  packageDocument,
  lockDocument,
  installRoot = E2E_ROOT,
) {
  const drift = [];
  const declared = {
    ...(packageDocument.dependencies ?? {}),
    ...(packageDocument.devDependencies ?? {}),
  };
  for (const name of Object.keys(declared).sort()) {
    if (!lockDocument.packages?.[`node_modules/${name}`]?.version) {
      drift.push(`node_modules/${name}: installed=unknown, locked=missing`);
    }
  }
  const lockedPackages = Object.entries(lockDocument.packages ?? {})
    .filter(([relative, entry]) => relative.includes('node_modules/') && entry?.version)
    .sort(([left], [right]) => left.localeCompare(right));
  for (const [relative, lockedEntry] of lockedPackages) {
    const locked = lockedEntry.version;
    let installed;
    try {
      installed = JSON.parse(
        fs.readFileSync(path.join(installRoot, relative, 'package.json')),
      ).version;
    } catch {
      installed = undefined;
    }
    if (installed === undefined && lockedEntry.optional === true) continue;
    if (typeof locked !== 'string' || installed !== locked) {
      drift.push(`${relative}: installed=${installed ?? 'missing'}, locked=${locked ?? 'missing'}`);
    }
  }
  if (drift.length > 0) {
    throw new Error(
      `E2E dependencies do not match package-lock.json (${drift.join('; ')}); run npm --prefix e2e ci`,
    );
  }
}

function verifyNpmDependencyTree() {
  const result = spawnSync('npm', ['ls', '--all', '--json'], {
    cwd: E2E_ROOT,
    encoding: 'utf8',
  });
  if (result.status !== 0) {
    let detail = result.stderr.trim();
    try {
      const document = JSON.parse(result.stdout);
      if (Array.isArray(document.problems) && document.problems.length > 0) {
        detail = document.problems.join('; ');
      }
    } catch {
      // Keep npm's diagnostic when it did not emit JSON.
    }
    throw new Error(
      `E2E dependency tree is invalid (${detail || `npm ls exited ${result.status}`}); run npm --prefix e2e ci`,
    );
  }
}

function argumentValue(name) {
  const index = process.argv.indexOf(name);
  if (index === -1) return undefined;
  if (!process.argv[index + 1]) throw new Error(`${name} requires a value`);
  return process.argv[index + 1];
}

export function runMain() {
  const packageDocument = loadPackage();
  verifyInstalledDependencies(
    packageDocument,
    JSON.parse(fs.readFileSync(path.join(E2E_ROOT, 'package-lock.json'), 'utf8')),
  );
  verifyNpmDependencyTree();
  const suiteNames = packageDocument.awakenTest?.deterministicSuites;
  if (!Array.isArray(suiteNames) || suiteNames.length === 0) {
    throw new Error('package.json awakenTest.deterministicSuites must be a non-empty array');
  }
  const commands = expandSuites(packageDocument.scripts, suiteNames);
  assertCanonicalPortOwnership(commands);
  const duplicate = commands.find((entry, index) => (
    commands.findIndex((candidate) => candidate.command === entry.command) !== index
  ));
  if (duplicate) throw new Error(`duplicate deterministic command: ${duplicate.command}`);

  const shard = parseShard(argumentValue('--shard') ?? process.env.AWAKEN_E2E_SHARD);
  const timingInput = argumentValue('--timings') ?? process.env.AWAKEN_E2E_TIMINGS_INPUT;
  const weights = timingInput
    ? timingWeights(JSON.parse(fs.readFileSync(path.resolve(REPO_ROOT, timingInput), 'utf8')))
    : new Map();
  const selected = assignShard(commands, shard, weights);
  const shardLabel = `${shard.index + 1}-of-${shard.total}`;
  const timingFile = path.resolve(
    REPO_ROOT,
    process.env.AWAKEN_E2E_TIMINGS_FILE
      ?? `target/test-timings/e2e-${shardLabel}.json`,
  );
  const report = {
    shard: { index: shard.index + 1, total: shard.total },
    startedAt: new Date().toISOString(),
    commands: [],
  };

  if (process.argv.includes('--prebuild-only') && !process.env.AWAKEN_E2E_PREBUILT_DIR) {
    throw new Error('--prebuild-only requires AWAKEN_E2E_PREBUILT_DIR');
  }
  const prebuildStarted = performance.now();
  const selectedPrebuiltDirectory = prebuiltDirectory(process.env.AWAKEN_E2E_PREBUILT_DIR);
  const environment = preparedEnvironment(process.env, selectedPrebuiltDirectory);
  report.prebuildDurationMs = Math.round(performance.now() - prebuildStarted);
  report.prebuilt = {
    awaken: environment[AWAKEN_BIN_ENV],
    scenarioHost: environment[SCENARIO_HOST_BIN_ENV],
    worker: environment[WORKER_BIN_ENV],
  };
  writeTimings(timingFile, report);
  if (process.argv.includes('--prebuild-only')) {
    console.log(`E2E prebuilt binaries: ${selectedPrebuiltDirectory}`);
    return;
  }

  console.log(`deterministic E2E shard ${shardLabel}: ${selected.length}/${commands.length} commands`);
  for (const [index, entry] of selected.entries()) {
    console.log(`\n[e2e ${index + 1}/${selected.length}] ${entry.command}`);
    const started = performance.now();
    const result = spawnSync(entry.command, {
      cwd: E2E_ROOT,
      env: environment,
      shell: true,
      stdio: 'inherit',
    });
    const timing = {
      command: entry.command,
      owner: entry.owner,
      durationMs: Math.round(performance.now() - started),
      status: result.status,
      signal: result.signal,
    };
    report.commands.push(timing);
    writeTimings(timingFile, report);
    if (result.error) throw result.error;
    if (result.status !== 0) {
      throw new Error(`${entry.command} failed with status ${result.status ?? result.signal}`);
    }
  }
  report.completedAt = new Date().toISOString();
  writeTimings(timingFile, report);
  console.log(`deterministic E2E timings: ${timingFile}`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    runMain();
  } catch (error) {
    console.error(`deterministic E2E runner failed: ${error.stack ?? error}`);
    process.exitCode = 1;
  }
}
