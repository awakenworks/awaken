// Portable MemoryStore projection end-to-end through the real execution-plane
// `awaken-sandbox memoryd` process. Three fresh mount directories model three pod
// generations over one durable SQLite resource store:
//
//   empty store -> create -> restart -> update/delete/create -> restart -> verify
//
// This is deliberately a process E2E rather than a Rust test. It proves that the
// no-FUSE realization uses the same durable MemoryRepository for materialization
// and harvest, and that a graceful sidecar shutdown commits the copy write set.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { REPO_ROOT } from './harness.mjs';

function ensureMemorydBin() {
  const output = execFileSync(
    'cargo',
    [
      'build',
      '--quiet',
      '--message-format=json',
      '-p',
      'awaken-sandbox',
      '--bin',
      'awaken-sandbox',
      '--features',
      'memoryd',
    ],
    { cwd: REPO_ROOT, maxBuffer: 64 * 1024 * 1024 },
  ).toString();
  for (const line of output.split('\n')) {
    if (!line.trim()) continue;
    try {
      const message = JSON.parse(line);
      if (message.executable && message.target?.name === 'awaken-sandbox') {
        return message.executable;
      }
    } catch {
      // Cargo diagnostics are not JSON messages with an executable.
    }
  }
  throw new Error('could not resolve the awaken-sandbox memoryd binary path');
}

function waitForExit(child) {
  return new Promise((resolve) => {
    child.once('exit', (code, signal) => resolve({ code, signal }));
  });
}

async function startGeneration(binary, storeDir, mountPath, { requestFuse = false } = {}) {
  fs.mkdirSync(mountPath, { recursive: true });
  const emptyPath = path.join(storeDir, 'empty-path');
  fs.mkdirSync(emptyPath, { recursive: true });
  const child = spawn(binary, ['memoryd'], {
    cwd: REPO_ROOT,
    env: {
      ...process.env,
      PATH: requestFuse ? emptyPath : process.env.PATH,
      AWAKEN_MEMORY_STORE_ID: 'memstore-e2e',
      AWAKEN_MEMORY_STORE_DIR: storeDir,
      AWAKEN_MOUNT_PATH: mountPath,
      AWAKEN_MEMORY_MODE: requestFuse ? 'fuse' : 'copy',
    },
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (chunk) => {
    stderr += chunk;
  });
  const exited = waitForExit(child);
  const deadline = Date.now() + 20_000;
  while (!stderr.includes('materialized')) {
    if (child.exitCode !== null) {
      throw new Error(`memoryd exited before materialization (${child.exitCode}): ${stderr}`);
    }
    if (Date.now() > deadline) {
      child.kill('SIGKILL');
      throw new Error(`timed out waiting for memoryd materialization: ${stderr}`);
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }

  return {
    mountPath,
    stderr: () => stderr,
    stop: async () => {
      child.kill('SIGTERM');
      const result = await exited;
      assert.equal(result.code, 0, `memoryd must stop cleanly: ${stderr}`);
      assert.match(stderr, /harvested \d+ changed memories/);
      return stderr;
    },
  };
}

async function runGeneration(binary, storeDir, mountPath, mutate, options = {}) {
  const generation = await startGeneration(binary, storeDir, mountPath, options);
  await mutate(generation.mountPath, generation.stderr());
  return generation.stop();
}

async function main() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-memoryd-copy-e2e-'));
  const storeDir = path.join(root, 'store');
  const binary = ensureMemorydBin();

  try {
    const missingConfig = spawnSync(binary, ['memoryd'], {
      cwd: REPO_ROOT,
      env: Object.fromEntries(
        Object.entries(process.env).filter(([name]) => !name.startsWith('AWAKEN_MEMORY_')),
      ),
      encoding: 'utf8',
    });
    assert.equal(missingConfig.status, 2);
    assert.match(missingConfig.stderr, /AWAKEN_MEMORY_STORE_ID is required/);

    const first = await runGeneration(
      binary,
      storeDir,
      path.join(root, 'mount-1'),
      async (mount) => {
        fs.writeFileSync(path.join(mount, 'root.md'), 'one');
        fs.mkdirSync(path.join(mount, 'notes', 'deep'), { recursive: true });
        fs.writeFileSync(path.join(mount, 'notes', 'deep', 'a.md'), 'nested-one');
        fs.writeFileSync(path.join(mount, 'not-memory.bin'), Buffer.from([0xff, 0xfe, 0xfd]));
      },
      { requestFuse: true },
    );
    assert.match(first, /FUSE requested .* falling back to copy mode/);
    assert.match(first, /harvested 2 changed memories/);

    const second = await runGeneration(
      binary,
      storeDir,
      path.join(root, 'mount-2'),
      async (mount) => {
        assert.equal(fs.readFileSync(path.join(mount, 'root.md'), 'utf8'), 'one');
        assert.equal(
          fs.readFileSync(path.join(mount, 'notes', 'deep', 'a.md'), 'utf8'),
          'nested-one',
        );
        assert.ok(!fs.existsSync(path.join(mount, 'not-memory.bin')));
        fs.writeFileSync(path.join(mount, 'root.md'), 'two');
        fs.rmSync(path.join(mount, 'notes', 'deep', 'a.md'));
        fs.mkdirSync(path.join(mount, 'fresh'), { recursive: true });
        fs.writeFileSync(path.join(mount, 'fresh', 'b.md'), 'new-two');
        fs.writeFileSync(path.join(mount, 'fresh', 'skip.bin'), Buffer.from([0xff, 0x00, 0xfe]));
      },
    );
    assert.match(second, /materialized 2 memories/);
    assert.match(second, /harvested 3 changed memories/);

    const third = await runGeneration(
      binary,
      storeDir,
      path.join(root, 'mount-3'),
      async (mount) => {
        assert.equal(fs.readFileSync(path.join(mount, 'root.md'), 'utf8'), 'two');
        assert.equal(fs.readFileSync(path.join(mount, 'fresh', 'b.md'), 'utf8'), 'new-two');
        assert.ok(!fs.existsSync(path.join(mount, 'notes', 'deep', 'a.md')));
        assert.ok(!fs.existsSync(path.join(mount, 'fresh', 'skip.bin')));
      },
    );
    assert.match(third, /materialized 2 memories/);
    assert.match(third, /harvested 0 changed memories/);

    // Two copy mounts can overlap in a container fleet. The first CAS writer wins;
    // the stale mount reports a conflict and never clobbers the durable head.
    const updateA = await startGeneration(binary, storeDir, path.join(root, 'mount-update-a'));
    const updateB = await startGeneration(binary, storeDir, path.join(root, 'mount-update-b'));
    fs.writeFileSync(path.join(updateA.mountPath, 'root.md'), 'winner');
    await updateA.stop();
    fs.writeFileSync(path.join(updateB.mountPath, 'root.md'), 'stale-loser');
    const staleUpdate = await updateB.stop();
    assert.match(staleUpdate, /preserved 1 concurrent durable heads/);

    // A path absent from both snapshots can be created concurrently. Exercise both
    // reconciliation outcomes: same bytes become idempotent, different bytes are an
    // explicit conflict with the already-durable writer winning.
    const pathA = await startGeneration(binary, storeDir, path.join(root, 'mount-path-a'));
    const pathB = await startGeneration(binary, storeDir, path.join(root, 'mount-path-b'));
    fs.writeFileSync(path.join(pathA.mountPath, 'race.md'), 'path-winner');
    await pathA.stop();
    fs.writeFileSync(path.join(pathB.mountPath, 'race.md'), 'path-loser');
    const staleCreate = await pathB.stop();
    assert.match(staleCreate, /preserved 1 concurrent durable heads/);

    const sameA = await startGeneration(binary, storeDir, path.join(root, 'mount-same-a'));
    const sameB = await startGeneration(binary, storeDir, path.join(root, 'mount-same-b'));
    fs.writeFileSync(path.join(sameA.mountPath, 'same.md'), 'same-content');
    await sameA.stop();
    fs.writeFileSync(path.join(sameB.mountPath, 'same.md'), 'same-content');
    const sameCreate = await sameB.stop();
    assert.doesNotMatch(sameCreate, /concurrent durable heads/);

    // A stale local deletion cannot remove a head updated by another mount.
    const deleteA = await startGeneration(binary, storeDir, path.join(root, 'mount-delete-a'));
    const deleteB = await startGeneration(binary, storeDir, path.join(root, 'mount-delete-b'));
    fs.writeFileSync(path.join(deleteA.mountPath, 'root.md'), 'latest');
    await deleteA.stop();
    fs.rmSync(path.join(deleteB.mountPath, 'root.md'));
    const staleDelete = await deleteB.stop();
    assert.match(staleDelete, /preserved 1 concurrent durable heads/);

    const finalGeneration = await runGeneration(
      binary,
      storeDir,
      path.join(root, 'mount-final'),
      async (mount) => {
        assert.equal(fs.readFileSync(path.join(mount, 'root.md'), 'utf8'), 'latest');
        assert.equal(fs.readFileSync(path.join(mount, 'race.md'), 'utf8'), 'path-winner');
        assert.equal(fs.readFileSync(path.join(mount, 'same.md'), 'utf8'), 'same-content');
      },
    );
    assert.match(finalGeneration, /harvested 0 changed memories/);

    console.log(
      'E2E PASS: memoryd copy mode persisted lifecycle changes and preserved concurrent durable heads.',
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error('E2E FAIL:', error);
  process.exitCode = 1;
});
