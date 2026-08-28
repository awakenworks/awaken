import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

import {
  RELEASE_HOSTED_ARGUMENTS,
  readDeploymentReplacementEvidence,
  replacementCommandEnvironment,
  runReleaseQualification,
} from './release-qualification.mjs';
import {
  awakenTargetFromEnvironment,
  officialReferenceFromEnvironment,
} from './hosted-configuration.mjs';

const directory = path.dirname(fileURLToPath(import.meta.url));

function run(command, arguments_, environment = process.env) {
  return new Promise((resolve, reject) => {
    const child = spawn(command, arguments_, { env: environment, stdio: 'inherit' });
    child.on('error', reject);
    child.on('exit', (code, signal) => {
      if (code === 0) resolve();
      else reject(new Error(
        `${path.basename(command)} failed (${signal ? `signal ${signal}` : `exit ${code}`})`,
      ));
    });
  });
}

export async function withReleaseArtifactDirectory(execute) {
  assert.equal(typeof execute, 'function', 'release artifact callback');
  const temporaryDirectory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-managed-release-'));
  let result;
  try {
    result = await execute(temporaryDirectory);
  } catch (error) {
    const failure = error instanceof Error ? error : new Error(String(error));
    failure.message = `${failure.message}; qualification artifacts preserved at ${temporaryDirectory}`;
    throw failure;
  }
  fs.rmSync(temporaryDirectory, { recursive: true, force: true });
  return result;
}

async function main() {
  assert.equal(process.argv.length, 2, 'release qualification accepts no arguments');
  const replaceCommand = process.env.AWAKEN_MANAGED_REPLACE_AND_WAIT_COMMAND;
  const expectedRevision = process.env.AWAKEN_MANAGED_EXPECTED_REVISION;
  const awaken = awakenTargetFromEnvironment(process.env);
  officialReferenceFromEnvironment(process.env, { requireReference: true });
  const expectedBaseURL = awaken.baseURL;
  assert.ok(replaceCommand, 'AWAKEN_MANAGED_REPLACE_AND_WAIT_COMMAND is required');
  assert.ok(
    path.isAbsolute(replaceCommand),
    'AWAKEN_MANAGED_REPLACE_AND_WAIT_COMMAND must be an absolute executable path',
  );
  assert.match(expectedRevision ?? '', /^[0-9A-Za-z][0-9A-Za-z._-]*$/u, 'exact deployed revision');

  await withReleaseArtifactDirectory(async (temporaryDirectory) => {
    const recoveryStateFile = path.join(temporaryDirectory, 'recovery.json');
    const replacementEvidenceFile = path.join(temporaryDirectory, 'replacement.json');
    const recoveryEnvironment = {
      ...process.env,
      AWAKEN_MANAGED_RECOVERY_STATE_FILE: recoveryStateFile,
    };
    const nodePhase = (script, ...arguments_) => run(
      process.execPath,
      [path.join(directory, script), ...arguments_],
      recoveryEnvironment,
    );
    await runReleaseQualification({
      hosted: async () => {
        for (const arguments_ of RELEASE_HOSTED_ARGUMENTS) {
          await nodePhase('hosted.mjs', ...arguments_);
        }
      },
      prepare: () => nodePhase('recovery.mjs', 'prepare'),
      replace: async () => {
        await run(replaceCommand, [], replacementCommandEnvironment(
          process.env,
          replacementEvidenceFile,
        ));
        readDeploymentReplacementEvidence(replacementEvidenceFile, {
          expectedRevision,
          expectedBaseURL,
        });
      },
      verify: () => nodePhase('recovery.mjs', 'verify'),
      cleanup: () => nodePhase('recovery.mjs', 'cleanup'),
    });
  });
  console.log('Managed release qualification passed with official differential and full process replacement');
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) await main();
