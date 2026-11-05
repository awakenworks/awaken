// Docker/creation-receipt unit evidence only. No SDK/model, Native process or
// live provider is started. TypeScript is the repository's existing parser;
// tests execute the ACTUAL private transition and original evidence writer.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import test from 'node:test';
import { spawnSync } from 'node:child_process';
import ts from 'typescript';
import { automatedAllInOneArgs } from './awaken_cli_args.mjs';

const driver = new URL('./managed_skill_lazy_real_e2e.mjs', import.meta.url);
const source = fs.readFileSync(driver, 'utf8');
const parsed = ts.createSourceFile(driver.pathname, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
function originalFunction(name) {
  const declarations = parsed.statements.filter(node => ts.isFunctionDeclaration(node) && node.name?.text === name);
  assert.equal(declarations.length, 1, `one original ${name} owner`);
  return declarations[0].getText(parsed);
}
const code = new vm.Script(`"use strict"; ${originalFunction('record')}
${originalFunction('restoreServiceContainer')}
restoreServiceContainer;`, { filename: driver.pathname });
const oldId = 'a'.repeat(64);
const newId = 'b'.repeat(64);
const otherId = 'c'.repeat(64);
const name = 'awaken-skill-lazy-fixture';

function fixture(t, { upgrade = true, run, inspect } = {}) {
  const output = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-lazy-cid-'));
  t.after(() => fs.rmSync(output, { recursive: true, force: true }));
  const before = { Id: oldId, Config: { Labels: { 'awaken.test.case': 'skill-lazy-real',
    'awaken.e2e.owner': 'fixture', 'awaken.e2e.run': 'fixture-run' },
    Entrypoint: ['/usr/local/bin/awaken'], Cmd: automatedAllInOneArgs('--config', '/etc/awaken/config.toml') },
  HostConfig: { NetworkMode: 'host', Binds: ['owned-config:/etc/awaken/config.toml:ro', 'owned-data:/var/lib/awaken'] } };
  const calls = [];
  const receipt = path.join(output, `runtime-replacement-${oldId}.cid`);
  const context = vm.createContext({ assert, fs, path, output, serviceContainer: name,
    initialServiceContainerId: oldId, serviceContainerId: oldId, directoryInit: true,
    upgradeImage: upgrade ? 'fixture:new' : undefined,
    evidence: { observations: [], service_container_id: oldId },
    execute: async (command, input, options) => {
      const args = Array.from(input);
      calls.push({ command, args, options });
      assert.equal(command, 'docker');
      if (args[0] === 'inspect') return inspect ? inspect(args) : { stdout: JSON.stringify([before]) };
      if (args[0] === 'run') {
        assert.equal(args[args.indexOf('--cidfile') + 1], receipt);
        assert.equal(args[args.indexOf('--name') + 1], name);
        if (run) return run({ args, receipt, before });
        fs.writeFileSync(receipt, newId, { flag: 'wx' });
        return { stdout: `${newId}\n` };
      }
      return { stdout: '' };
    } });
  return { restore: code.runInContext(context), context, calls, receipt, before, output };
}

// R1 no upgrade => inspect/restart the selected immutable CID only. The name
// remains a creation coordinate and never becomes a discovery fallback.
test('restart retains its input CID and never selects by name', async t => {
  const f = fixture(t, { upgrade: false });
  await f.restore();
  assert.equal(f.calls[0].args.at(-1), oldId);
  assert.deepEqual(f.calls.slice(1).map(call => call.args), [['restart', '--time', '15', oldId]]);
  assert.equal(f.context.serviceContainerId, oldId);
  assert.equal(f.context.directoryInit, true);
  assert.equal(fs.existsSync(f.receipt), false);
});

// R2 upgrade + actual CID receipt => preserve original stop/remove/create
// order and original binds/command; hand off the NEW physical identity to the
// same evidence/cleanup owner. It does not prove the product is Ready.
test('upgrade preserves construction and records the actual new CID', async t => {
  const f = fixture(t);
  await f.restore();
  assert.equal(f.calls[0].args.at(-1), oldId);
  assert.deepEqual(f.calls.slice(1, 3).map(call => call.args), [['stop', '--time', '15', oldId], ['rm', '-v', oldId]]);
  const args = f.calls[3].args;
  assert.deepEqual(args.slice(-f.before.Config.Cmd.length), f.before.Config.Cmd);
  for (const binding of f.before.HostConfig.Binds) assert.ok(args.includes(binding));
  assert.ok(args.includes('awaken.e2e.owner=fixture') && args.includes('awaken.e2e.run=fixture-run'));
  assert.equal(f.context.serviceContainerId, newId);
  assert.equal(f.context.directoryInit, false);
  assert.equal(fs.readFileSync(f.receipt, 'utf8'), newId);
  const actual = JSON.parse(fs.readFileSync(path.join(f.output, 'evidence.json'), 'utf8'));
  assert.equal(actual.service_container_id, newId);
  assert.equal(actual.observations[0].previous_container_id, oldId);
});

// R3 creation fails AFTER Docker wrote its actual receipt => retain that CID
// for cleanup while preserving the original error; no success/Ready is inferred.
test('failed start retains the created CID without swallowing its error', async t => {
  const refusal = Object.assign(new Error('Docker start refused'), { code: 37 });
  const f = fixture(t, { run: ({ receipt }) => { fs.writeFileSync(receipt, newId); throw refusal; } });
  await assert.rejects(f.restore, error => error === refusal);
  assert.equal(f.context.serviceContainerId, newId);
  assert.equal(f.context.evidence.service_container_id, newId);
  assert.equal(f.context.directoryInit, true);
});

// R4 failed create BEFORE any receipt => do not invent a replacement or inspect
// the reused name. A separate ownership/cleanup refusal must keep unknown data.
test('failed create without a receipt never discovers a replacement by name', async t => {
  const refusal = Object.assign(new Error('name already occupied'), { code: 37 });
  const f = fixture(t, { run: () => { throw refusal; } });
  await assert.rejects(f.restore, error => error === refusal);
  assert.equal(f.context.serviceContainerId, oldId);
  assert.equal(fs.existsSync(f.receipt), false);
  assert.deepEqual(f.calls.filter(call => call.args[0] === 'inspect').map(call => call.args.at(-1)), [oldId]);
});

// R5 successful CLI reply is insufficient: missing/empty/bad/symlink receipts
// refuse; a valid receipt with a mismatched reply stays available for cleanup
// but cannot qualify the transition. These are independent output partitions.
for (const fault of ['missing', 'empty', 'bad', 'symlink', 'dangling-symlink', 'different-reply']) {
  test(`creation receipt ${fault} cannot qualify a successful transition`, async t => {
    const f = fixture(t, { run: ({ receipt }) => {
      if (fault === 'empty') fs.writeFileSync(receipt, '');
      else if (fault === 'bad') fs.writeFileSync(receipt, 'awaken-skill-lazy-another');
      else if (fault === 'symlink' || fault === 'dangling-symlink') {
        const target = `${receipt}.target`; if (fault === 'symlink') fs.writeFileSync(target, newId); fs.symlinkSync(target, receipt);
      } else if (fault === 'different-reply') fs.writeFileSync(receipt, newId);
      return { stdout: fault === 'different-reply' ? otherId : newId };
    } });
    await assert.rejects(f.restore);
    assert.equal(f.context.serviceContainerId, fault === 'different-reply' ? newId : oldId);
    assert.equal(f.context.directoryInit, true);
  });
}

// R6 old CID is gone while the name can identify another object => inspect
// fails at that CID, and no stop/remove/restart/create is attempted on the name.
test('a foreign replacement name is never followed after the retained CID is gone', async t => {
  const missing = new Error('No such retained object');
  const f = fixture(t, { inspect: args => { assert.equal(args.at(-1), oldId); throw missing; } });
  await assert.rejects(f.restore, error => error === missing);
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].args[0], 'inspect');
  assert.equal(f.calls[0].args.at(-1), oldId);
});

// R7 a stale receipt predates this attempt => refuse BEFORE stopping the old
// process, and do not adopt/delete the unrelated CID inside that file.
test('a pre-existing receipt cannot authorize a replacement', async t => {
  const f = fixture(t);
  fs.writeFileSync(f.receipt, otherId);
  await assert.rejects(f.restore, /receipt is fresh/);
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].args[0], 'inspect');
  assert.equal(f.calls[0].args.at(-1), oldId);
  assert.equal(f.context.serviceContainerId, oldId);
  assert.equal(fs.readFileSync(f.receipt, 'utf8'), otherId);
});

// R8 both command and receipt fail => preserve the FIRST command failure and
// record the independent receipt refusal, never replace it with a fake CID.
test('malformed receipt cannot replace the original failed-create error', async t => {
  const refusal = Object.assign(new Error('original create failure'), { code: 37 });
  const f = fixture(t, { run: ({ receipt }) => { fs.writeFileSync(receipt, 'bad'); throw refusal; } });
  await assert.rejects(f.restore, error => error === refusal);
  assert.equal(f.context.serviceContainerId, oldId);
  assert.ok(f.context.evidence.service_container_receipt_error);
});

// R7b the freshness check also sees dangling symlinks, before any process
// mutation; existsSync alone would silently treat that existing path as absent.
test('a pre-existing dangling receipt is rejected before stopping the runtime', async t => {
  const f = fixture(t);
  // NTFS junctions retain the dangling reparse point without symlink privileges.
  fs.symlinkSync(`${f.receipt}.absent`, f.receipt, process.platform === 'win32' ? 'junction' : 'file');
  assert.equal(fs.lstatSync(f.receipt).isSymbolicLink(), true);
  assert.throws(() => fs.statSync(f.receipt), { code: 'ENOENT' });
  await assert.rejects(f.restore, /receipt is fresh/);
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].args[0], 'inspect');
  assert.equal(f.calls[0].args.at(-1), oldId);
  assert.equal(fs.lstatSync(f.receipt).isSymbolicLink(), true);
});

// Build-input decision table, separate from the CID transition evidence above:
// C1 default/explicit runtime image; C2 each exact role resolves/fails; C3 a
// nondefault target has stale target/release decoys; C4 alias copy/image fails.
// Effects: E1 stage CLI/Coordinator/Worker snapshots with the exact feature and
// release inputs, reusing CLI bytes for awaken-server; E2 preserve the original
// failure before later image/cluster work; E3 explicit image performs no build.
// B1 C1=default+C2=all healthy+C3 => E1; B2 any C2 failure or C4 => E2;
// B3 C1=explicit => E3. Execute the ACTUAL inline shell branch between its two
// fixed surrounding statements. Only resolver/Cargo/copy/image IO is replaced;
// the final marker observes continuation, not a fake cluster or product Ready.
const shellSource = fs.readFileSync(new URL('./k3d/managed_skill_lazy_real_e2e.sh', import.meta.url), 'utf8');
const buildStart = 'if [[ -z "${AWAKEN_LAZY_RUNTIME_IMAGE:-}" ]]; then\n';
const buildEnd = '\nfi\n[[ "$(docker inspect';
assert.equal(shellSource.split(buildStart).length, 2, 'one original runtime build branch');
assert.equal(shellSource.split(buildEnd).length, 2, 'one original post-build boundary');
const buildBegin = shellSource.indexOf(buildStart);
const buildFinish = shellSource.indexOf(buildEnd, buildBegin);
assert.ok(buildFinish > buildBegin, 'original branch boundaries remain ordered');
const originalBuildBranch = shellSource.slice(buildBegin, buildFinish + '\nfi'.length);

function fixtureBash() {
  const configured = process.env.AWAKEN_NATIVE_BASH;
  if (configured) {
    assert.ok(path.isAbsolute(configured) && fs.statSync(configured).isFile(), 'AWAKEN_NATIVE_BASH must name an existing absolute native Bash');
    return configured;
  }
  if (process.platform !== 'win32') return 'bash';
  const roots = [process.env.ProgramFiles, process.env['ProgramFiles(x86)'],
    process.env.LOCALAPPDATA && path.join(process.env.LOCALAPPDATA, 'Programs')].filter(Boolean);
  const native = roots.map(root => path.join(root, 'Git', 'bin', 'bash.exe')).find(candidate => fs.existsSync(candidate));
  assert.ok(native, 'Set AWAKEN_NATIVE_BASH to the absolute native Git Bash executable');
  return native;
}

function shellBuildFixture(t, { failure = '', explicitImage = false } = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-lazy-build-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const context = path.join(root, 'runtime image context');
  fs.mkdirSync(context);
  const decoys = path.join(root, 'target', 'release');
  fs.mkdirSync(decoys, { recursive: true });
  for (const name of ['awaken', 'awaken-coordinator', 'awaken-worker']) {
    fs.writeFileSync(path.join(decoys, name), `stale:${name}`);
  }
  const calls = path.join(root, 'io.jsonl');
  fs.writeFileSync(calls, '');
  const stub = path.join(root, 'io.mjs');
  fs.writeFileSync(stub, `import fs from 'node:fs';
const [operation, ...args] = process.argv.slice(2);
fs.appendFileSync(process.env.LAZY_IO_CALLS, JSON.stringify({ operation, args, target: process.env.CARGO_TARGET_DIR })+'\\n');
if (operation === 'resolve') {
  const failure = new Map([['awaken-cli',41],['awaken-coordinator-process',42],['awaken-worker',43]]);
  if (process.env.LAZY_IO_FAILURE === args[0]) process.exit(failure.get(args[0]));
  const destination = args[args.indexOf('--snapshot-destination') + 1];
  if (!args.includes('--snapshot-destination')) process.exit(98);
  fs.writeFileSync(destination, 'selected:'+args[0]+'/'+args[1], { mode: 0o755 });
} else if (operation === 'copy') {
  if (process.env.LAZY_IO_FAILURE === 'copy') process.exit(29);
  fs.copyFileSync(args[0], args[1]);
} else if (operation === 'image' && process.env.LAZY_IO_FAILURE === 'image') process.exit(37);
`);
  const input = `set -euo pipefail
resolve_cargo_executable() { "$LAZY_IO_NODE" "$LAZY_IO_STUB" resolve "$@"; }
cargo() { "$LAZY_IO_NODE" "$LAZY_IO_STUB" cargo "$@"; }
cp() { "$LAZY_IO_NODE" "$LAZY_IO_STUB" copy "$@"; }
k3d_docker_build() { "$LAZY_IO_NODE" "$LAZY_IO_STUB" image "$@"; }
${originalBuildBranch}
"$LAZY_IO_NODE" "$LAZY_IO_STUB" continuation
`;
  // Stdin preserves quotes/newlines without the Windows CRT/MSYS argv mismatch.
  const result = spawnSync(fixtureBash(), ['-s'], { input, windowsHide: true, cwd: root, encoding: 'utf8', env: { ...process.env,
    CARGO_TARGET_DIR: path.join(root, 'nondefault final target'),
    AWAKEN_LAZY_RUNTIME_IMAGE: explicitImage ? 'fixture:already-built' : '',
    LAZY_TEMP: context, LAZY_COMMIT: 'a'.repeat(40), LAZY_IMAGE: 'fixture:runtime',
    LAZY_IO_NODE: process.execPath, LAZY_IO_STUB: stub, LAZY_IO_CALLS: calls,
    LAZY_IO_FAILURE: failure } });
  assert.ifError(result.error);
  return { result, context, decoys,
    calls: fs.readFileSync(calls, 'utf8').split('\n').filter(Boolean).map(JSON.parse),
    target: path.join(root, 'nondefault final target') };
}

test('default image stages all exact role snapshots instead of stale target paths', t => {
  const f = shellBuildFixture(t);
  assert.equal(f.result.status, 0, f.result.stderr);
  assert.equal(fs.readFileSync(path.join(f.context, 'awaken'), 'utf8'), 'selected:awaken-cli/awaken', 'B1 current CLI, not the stale decoy');
  assert.equal(fs.readFileSync(path.join(f.context, 'awaken-server'), 'utf8'), 'selected:awaken-cli/awaken', 'B1 same CLI snapshot alias');
  assert.equal(fs.readFileSync(path.join(f.context, 'awaken-coordinator'), 'utf8'), 'selected:awaken-coordinator-process/awaken-coordinator', 'B1 independently declared Coordinator');
  assert.equal(fs.readFileSync(path.join(f.context, 'awaken-worker'), 'utf8'), 'selected:awaken-worker/awaken-worker', 'B1 Worker');
  const roles = [['awaken-cli', 'awaken'], ['awaken-coordinator-process', 'awaken-coordinator'], ['awaken-worker', 'awaken-worker']];
  assert.deepEqual(f.calls.filter(call => call.operation === 'resolve'), roles.map(([owner, binary]) => ({
    // The actual Bash branch appends '/' even when its injected root is native Windows text.
    operation: 'resolve', args: [owner, binary, '--release', '--features', 'container-k8s', '--snapshot-destination', path.posix.join(f.context, binary)], target: f.target,
  })), 'B1 exact role/feature/profile/snapshot inputs');
  assert.equal(f.calls.some(call => call.operation === 'cargo'), false, 'B1 no second Cargo path');
  assert.deepEqual(f.calls.filter(call => call.operation === 'copy').map(call => call.args), [[path.posix.join(f.context, 'awaken'), path.posix.join(f.context, 'awaken-server')]], 'B1 one alias copy');
  assert.deepEqual(f.calls.filter(call => call.operation === 'image').map(call => call.args), [[
    '--load', '-f', 'deploy/k3d/Dockerfile', '--build-arg', 'AUXILIARY_EXECUTABLE=awaken',
    '--label', `org.opencontainers.image.revision=${'a'.repeat(40)}`, '-t', 'fixture:runtime', f.context,
  ]], 'B1 original image inputs');
  assert.equal(f.calls.at(-1).operation, 'continuation');
  for (const [, binary] of roles) assert.equal(fs.readFileSync(path.join(f.decoys, binary), 'utf8'), `stale:${binary}`, 'B1 decoys untouched');
});

for (const [owner, status] of [['awaken-cli', 41], ['awaken-coordinator-process', 42], ['awaken-worker', 43]]) {
  test(`${owner} resolution failure preserves its code before image or cluster work`, t => {
    const f = shellBuildFixture(t, { failure: owner });
    assert.equal(f.result.status, status, 'B2 original resolver code');
    assert.equal(f.calls.at(-1).args[0], owner, 'B2 failed selected role is the last operation');
    assert.equal(f.calls.some(call => ['image', 'continuation'].includes(call.operation)), false, 'B2 no later effects');
  });
}

for (const [failure, status] of [['copy', 29], ['image', 37]]) {
  test(`runtime ${failure} failure preserves its code and blocks continuation`, t => {
    const f = shellBuildFixture(t, { failure });
    assert.equal(f.result.status, status, 'B2 original IO code');
    assert.equal(f.calls.at(-1).operation, failure, 'B2 no later operation');
    assert.equal(f.calls.some(call => call.operation === 'continuation'), false, 'B2 no cluster handoff');
    if (failure === 'copy') assert.equal(f.calls.some(call => call.operation === 'image'), false);
  });
}

test('an explicit runtime image skips all host role builds and staging', t => {
  const f = shellBuildFixture(t, { explicitImage: true });
  assert.equal(f.result.status, 0, f.result.stderr);
  assert.deepEqual(f.calls, [{ operation: 'continuation', args: [], target: f.target }], 'B3 original override branch');
  assert.deepEqual(fs.readdirSync(f.context), [], 'B3 no new build artifacts');
});
