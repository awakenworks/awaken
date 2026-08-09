import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';

import {
  availablePort,
  stopServer,
  trackSpawnedServer,
  waitForPort,
} from './harness.mjs';

async function main() {
  // Cause/effect graph: port ownership (tracked child / explicit child / no
  // child) × child state (listening / terminal) -> readiness or bounded error.
  // Decision table: R1 tracked+terminal -> immediate child-exit error; R2
  // explicit+terminal -> same error; R3 external/unowned -> deadline error;
  // R4 tracked+listening -> readiness success. These rules prevent a crashed
  // E2E server from being reported only after the 15-minute port deadline.
  const trackedPort = await availablePort(21_000);
  const trackedExit = trackSpawnedServer(
    trackedPort,
    spawn(process.execPath, ['-e', 'process.exit(23)'], { stdio: 'ignore' }),
  );
  await new Promise((resolve) => trackedExit.once('exit', resolve));
  await assert.rejects(
    waitForPort(trackedPort, 5_000),
    /server exited before it listened.*code=23/u,
    'R1',
  );

  const explicitPort = await availablePort(trackedPort + 1);
  const explicitExit = spawn(process.execPath, ['-e', 'process.exit(24)'], { stdio: 'ignore' });
  await assert.rejects(
    waitForPort(explicitPort, 5_000, explicitExit),
    /server exited before it listened.*code=24/u,
    'R2',
  );

  const spawnFailurePort = await availablePort(explicitPort + 1);
  const spawnFailure = spawn('/definitely-not-an-awaken-e2e-binary', [], { stdio: 'ignore' });
  await assert.rejects(
    waitForPort(spawnFailurePort, 5_000, spawnFailure),
    /server exited before it listened.*spawn=ENOENT/u,
    'R2b',
  );
  await stopServer(spawnFailure);

  const externalPort = await availablePort(spawnFailurePort + 1);
  await assert.rejects(
    waitForPort(externalPort, 50),
    /server did not listen/u,
    'R3',
  );

  const listeningPort = await availablePort(externalPort + 1);
  const listening = trackSpawnedServer(
    listeningPort,
    spawn(
      process.execPath,
      [
        '-e',
        `require('node:net').createServer().listen(${listeningPort}, '127.0.0.1')`,
      ],
      { stdio: 'ignore' },
    ),
  );
  try {
    await waitForPort(listeningPort, 5_000);
  } finally {
    await stopServer(listening);
  }

  await Promise.all([stopServer(trackedExit), stopServer(explicitExit)]);
  console.log('E2E PASS: harness child lifecycle readiness decision table.');
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
