// Docker/creation-receipt unit evidence only. No SDK/model, Native process or
// live provider is started. TypeScript is the repository's existing parser;
// tests execute the ACTUAL private transition and original evidence writer.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import vm from 'node:vm';
import test from 'node:test';
import ts from 'typescript';

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
    Entrypoint: ['/usr/local/bin/awaken'], Cmd: ['all-in-one', '--config', '/etc/awaken/config.toml'] },
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
  fs.symlinkSync(`${f.receipt}.absent`, f.receipt);
  await assert.rejects(f.restore, /receipt is fresh/);
  assert.equal(f.calls.length, 1);
  assert.equal(f.calls[0].args[0], 'inspect');
  assert.equal(f.calls[0].args.at(-1), oldId);
  assert.equal(fs.lstatSync(f.receipt).isSymbolicLink(), true);
});
