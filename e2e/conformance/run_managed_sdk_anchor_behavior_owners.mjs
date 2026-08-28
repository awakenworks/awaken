// Replay the single Managed behavior-owner graph through every exact supported
// TypeScript SDK anchor. The child runner derives each historical operation
// subset from that package's generated source; this orchestrator owns versions
// and process isolation only, never a second operation or scenario list.

import { spawn } from 'node:child_process';
import { resolve } from 'node:path';
import {
  installedPackage,
  readSdkMatrix,
  validateSdkMatrix,
} from '../../packages/managed-sdk-oracle/src/conformance/clients.mjs';

const runner = resolve(import.meta.dirname, 'run_managed_sdk_behavior_owners.mjs');
const matrix = validateSdkMatrix(readSdkMatrix());
const {
  ANTHROPIC_SDK_CONFORMANCE_CANDIDATE: _candidateModule,
  ANTHROPIC_SDK_CONFORMANCE_CANDIDATE_VERSION: _candidateVersion,
  ...baseEnvironment
} = process.env;

for (const [index, anchor] of matrix.entries()) {
  const sdk = installedPackage(anchor.module);
  console.log(
    `[managed-sdk anchor ${index + 1}/${matrix.length}] ${anchor.role} ${sdk.version}`,
  );
  await new Promise((resolveAnchor, rejectAnchor) => {
    const child = spawn(process.execPath, [runner], {
      env: {
        ...baseEnvironment,
        ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT: sdk.root,
        AWAKEN_MANAGED_SDK_EXPECTED_VERSION: sdk.version,
        AWAKEN_MANAGED_SDK_HISTORICAL_SUBSET:
          anchor.role === 'current_oracle' ? '0' : '1',
      },
      stdio: 'inherit',
    });
    child.once('error', rejectAnchor);
    child.once('close', (status, signal) => {
      if (signal) rejectAnchor(new Error(`${anchor.id}: behavior owners terminated by ${signal}`));
      else if (status !== 0) {
        rejectAnchor(new Error(`${anchor.id}: behavior owners exited with status ${status}`));
      } else resolveAnchor();
    });
  });
}

console.log(`Managed SDK anchor behavior matrix PASS: ${matrix.length} exact versions.`);
