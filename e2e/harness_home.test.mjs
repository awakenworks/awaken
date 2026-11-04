import assert from 'node:assert/strict';
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import vm from 'node:vm';
import ts from 'typescript';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { inflateSync } from 'node:zlib';

test('independent harness instances own distinct native homes and clean only those homes', (t) => {
  // C1 caller-owned native temp root; C2 a pre-existing foreign sibling;
  // C3 two independent imports in one PID; C4 normal child exit. Effects:
  // R1 each import allocates a new root under C1; R2 C2 and the first import's
  // sentinel survive the second import; R3 C4 removes both allocated roots
  // while preserving the sibling. No server or shared build is required.
  const root = fs.mkdtempSync(path.join(tmpdir(), 'harness-home-law-'));
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));
  const sibling = path.join(root, 'awaken-e2e-home-foreign');
  fs.mkdirSync(sibling);
  fs.writeFileSync(path.join(sibling, 'sentinel'), 'foreign');
  const harness = new URL('./harness.mjs', import.meta.url).href;
  const script = `
    import assert from 'node:assert/strict';
    import fs from 'node:fs';
    import path from 'node:path';
    const base = ${JSON.stringify(root)};
    const first = await import(${JSON.stringify(harness + '?home-law=first')});
    assert.equal(path.dirname(first.E2E_HOME_ROOT), base);
    assert.ok(path.basename(first.E2E_HOME_ROOT).startsWith('awaken-e2e-home-'));
    fs.writeFileSync(path.join(first.E2E_HOME_ROOT, 'sentinel'), 'first');
    const second = await import(${JSON.stringify(harness + '?home-law=second')});
    assert.equal(path.dirname(second.E2E_HOME_ROOT), base);
    assert.notEqual(second.E2E_HOME_ROOT, first.E2E_HOME_ROOT);
    assert.equal(fs.readFileSync(path.join(first.E2E_HOME_ROOT, 'sentinel'), 'utf8'), 'first');
    assert.equal(fs.readFileSync(path.join(base, 'awaken-e2e-home-foreign', 'sentinel'), 'utf8'), 'foreign');
    for (const item of [first, second]) {
      assert.equal(item.E2E_HOME, path.join(item.E2E_HOME_ROOT, 'home'));
      const config = fs.readFileSync(path.join(item.E2E_HOME, '.awaken/config.toml'), 'utf8');
      assert.ok(config.includes('data_dir = ' + JSON.stringify(item.E2E_HOME_ROOT + '/data')));
    }
    console.log(JSON.stringify([first.E2E_HOME_ROOT, second.E2E_HOME_ROOT]));
  `;
  const child = spawnSync(process.execPath, ['--input-type=module', '-e', script], {
    cwd: path.dirname(fileURLToPath(import.meta.url)),
    env: { ...process.env, TEMP: root, TMP: root, TMPDIR: root, E2E_PORT: '1', E2E_WORKER_PORT: '2' },
    encoding: 'utf8', timeout: 30_000,
  });
  assert.ifError(child.error);
  assert.equal(child.status, 0, child.stderr);
  assert.equal(child.signal, null);
  const homes = JSON.parse(child.stdout.trim());
  assert.equal(homes.length, 2);
  assert.equal(new Set(homes).size, 2);
  for (const home of homes) {
    assert.equal(path.dirname(home), root);
    assert.equal(fs.existsSync(home), false, 'child exit removes only its allocated home');
  }
  assert.equal(fs.readFileSync(path.join(sibling, 'sentinel'), 'utf8'), 'foreign');
  assert.deepEqual(fs.readdirSync(root), ['awaken-e2e-home-foreign']);
});

test('the shared offline image decodes to a conventional solid-red RGB PNG', async () => {
  // One immutable fixture has no branching input, so binary-format inspection
  // is the smallest complete test design. Causes: C1 exported base64/data URI;
  // C2 PNG chunk bytes and checksums; C3 compressed image scanlines. Effects:
  // R1 both exports carry identical bytes; R2 standard 8-bit, non-interlaced RGB
  // at 256x256 with valid lengths/CRCs; R3 every decoded pixel is red. This
  // checks the image semantics without a network service or model credentials.
  const { RED_PNG_B64, RED_PNG_DATA_URI } = await import('./harness.mjs');
  const image = Buffer.from(RED_PNG_B64, 'base64');
  assert.equal(RED_PNG_DATA_URI, `data:image/png;base64,${RED_PNG_B64}`, 'R1');
  assert.deepEqual(image.subarray(0, 8), Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]));
  const chunks = [];
  for (let offset = 8; offset < image.length;) {
    assert.ok(offset + 12 <= image.length, 'R2 complete chunk header/trailer');
    const length = image.readUInt32BE(offset);
    const end = offset + 12 + length;
    assert.ok(end <= image.length, 'R2 complete chunk payload');
    const type = image.toString('ascii', offset + 4, offset + 8);
    let crc = 0xffffffff;
    for (const byte of image.subarray(offset + 4, end - 4)) {
      crc ^= byte;
      for (let bit = 0; bit < 8; bit++) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
    }
    assert.equal(image.readUInt32BE(end - 4), (crc ^ 0xffffffff) >>> 0, `R2 ${type} CRC`);
    chunks.push({ type, data: image.subarray(offset + 8, end - 4) });
    offset = end;
  }
  assert.deepEqual(chunks.map(chunk => chunk.type), ['IHDR', 'IDAT', 'IEND']);
  assert.deepEqual([...chunks[0].data], [0, 0, 1, 0, 0, 0, 1, 0, 8, 2, 0, 0, 0], 'R2 RGB header');
  assert.equal(chunks[2].data.length, 0);
  const pixels = inflateSync(chunks[1].data);
  const row = Buffer.from([0, ...Array.from({ length: 256 }, () => [255, 0, 0]).flat()]);
  assert.equal(pixels.length, row.length * 256, 'R3 exact decompressed size');
  for (let y = 0; y < 256; y++) {
    assert.deepEqual(pixels.subarray(y * row.length, (y + 1) * row.length), row, `R3 row ${y}`);
  }
});

test('canonical fixture removal preserves foreign siblings and is idempotent', async (t) => {
  // C1 isolated caller tree with the observed late SDK config leaf; C2 foreign
  // sibling; C3 repeat after absence. R1 remove only C1, R2 C2 survives both
  // calls, R3 absent C1 succeeds. The real Claude E2E owns the observed transient
  // ENOTEMPTY regression; Node owns bounded retry, not a second removal loop.
  const root=fs.mkdtempSync(path.join(tmpdir(),'fixture-removal-law-'));
  t.after(()=>fs.rmSync(root,{recursive:true,force:true}));
  const own=path.join(root,'own'), foreign=path.join(root,'foreign');
  fs.mkdirSync(path.join(own,'e2e-home'),{recursive:true});fs.mkdirSync(foreign);
  fs.writeFileSync(path.join(own,'e2e-home','.claude.json'),'synthetic');
  fs.writeFileSync(path.join(foreign,'sentinel'),'keep');
  const {cleanupFixtureTree}=await import('./harness.mjs');
  cleanupFixtureTree(own);cleanupFixtureTree(own);
  assert.equal(fs.existsSync(own),false);
  assert.equal(fs.readFileSync(path.join(foreign,'sentinel'),'utf8'),'keep');
});

// Execute the actual wrapper with declared construction leaves, a real owned
// HTTP fixture and a real Node child. Native/Cargo are not started here.
// C1 failure before/after upstream acquisition; C2 child absent/created;
// C3 readiness/body failure; C4 stop/close rejection. Effects: close every
// acquired resource in dependency order, await close, preserve a lone error's
// identity or every aggregate cause, and return the unchanged successful value.
const wrapperSource = fs.readFileSync(new URL('./harness.mjs', import.meta.url), 'utf8');
const wrapperAst = ts.createSourceFile('harness.mjs', wrapperSource, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
const wrapperDeclaration = wrapperAst.statements.find(node => ts.isFunctionDeclaration(node) && node.name?.text === 'withRealServer');
assert.ok(wrapperDeclaration);

async function exerciseWireWrapper(t, stage, cleanupMask = 0, strict = false) {
  const { startFakeAnthropic } = await import('./fixtures/fake_anthropic_fixture.mjs');
  const { stopServer, runCleanupSteps } = await import('./harness.mjs');
  const primary = new Error(`controlled ${stage} failure`);
  const stopError = new Error('controlled stop failure');
  const closeError = new Error('controlled upstream close failure');
  const value = { unchanged: true };
  const calls = [];
  let upstream, child, ready;
  let closeAcknowledged = false;
  t.after(async () => {
    if (child) await stopServer(child);
    if (upstream) await upstream.close();
  });
  const context = vm.createContext({ process, runCleanupSteps, AggregateError,
    FAKE_KEY: 'fixture-key',
    ensureBuilt: () => { if (stage === 'ensure') throw primary; },
    startFakeAnthropic: async (...args) => {
      if (stage === 'upstream') throw primary;
      upstream = await startFakeAnthropic(...args);
      return { ...upstream, close: async () => {
        calls.push('close');
        await upstream.close();
        await new Promise(resolve => setImmediate(resolve));
        closeAcknowledged = true;
        if (cleanupMask & 2) throw closeError;
      } };
    },
    availablePort: async () => { if (stage === 'port') throw primary; return 1; },
    realServerEnv: () => { if (stage === 'environment') throw primary; return { AWAKEN_MODEL_MODE: 'fixture' }; },
    spawnServer: () => {
      if (stage === 'spawn') throw primary;
      child = spawn(process.execPath, ['-e', "process.stdin.resume();process.stdin.once('end',()=>process.exit(0));process.send('ready')"],
        { stdio: ['pipe', 'ignore', 'ignore', 'ipc'] });
      ready = once(child, 'message', { signal: AbortSignal.timeout(5000) });
      return { server: child, baseUrl: 'http://127.0.0.1:1' };
    },
    waitForServer: async () => { await ready; if (stage === 'readiness') throw primary; },
    stopServer: async (server, options) => {
      calls.push('stop');
      assert.equal(options?.requireCleanExit ?? false, strict, 'the wrapper forwards the selected existing shutdown contract');
      await stopServer(server, options);
      if (cleanupMask & 1) throw stopError;
    },
  });
  const declaration = wrapperDeclaration.getText(wrapperAst).replace(/^export /u, '');
  const actual = new vm.Script(`${declaration}\nwithRealServer;`).runInContext(context);
  const outcome = actual('echo', 1, async (_base, peer) => {
    assert.equal(peer.url, upstream.url);
    if (stage === 'body') throw primary;
    return value;
  }, { requireCleanExit: strict });
  const expected = stage === 'success' ? [] : [primary];
  if (['readiness', 'body', 'success'].includes(stage) && (cleanupMask & 1)) expected.push(stopError);
  if (!['ensure', 'upstream'].includes(stage) && (cleanupMask & 2)) expected.push(closeError);
  if (expected.length) {
    await assert.rejects(outcome, error => {
      if (expected.length === 1) assert.equal(error, expected[0], 'a lone error retains its existing identity');
      else assert.deepEqual(Array.from(error.errors), expected, 'all original and cleanup causes are retained');
      return true;
    });
  } else assert.equal(await outcome, value);
  if (upstream) {
    assert.equal(closeAcknowledged, true, 'owned upstream closure is awaited even when preparation throws');
    const closed = await fetch(`${upstream.url}/v1/models`).then(() => false, error => error.cause?.code === 'ECONNREFUSED');
    assert.equal(closed, true, 'the exact upstream listener no longer accepts requests');
  }
  assert.deepEqual(calls, child ? ['stop', 'close'] : upstream ? ['close'] : []);
  if (child) assert.equal(child.exitCode, 0);
}

test('withRealServer closes its upstream on a preparation failure', async t => {
  // R1: upstream acquired + late server preparation/Hand build failure =>
  // propagate the original error and await upstream close; no live listener.
  await exerciseWireWrapper(t, 'spawn');
});

test('withRealServer preserves results and all cleanup outcomes', async t => {
  // R2 acquisition/readiness/body partitions; R3 successful/body-failed calls
  // crossed with both cleanup failures. Stop precedes close, but a failed stop
  // cannot prevent close. No foreign listener or native process is involved.
  for (const stage of ['ensure', 'upstream', 'port', 'environment', 'readiness', 'body', 'success']) {
    await t.test(stage, child => exerciseWireWrapper(child, stage));
  }
  // R4 strict acceptance uses the same real child's EOF -> exit -> stdio close;
  // a wrapper must forward that choice, while ordinary cleanup keeps its original
  // idempotent/non-strict behavior and original aggregated failures.
  await t.test('strict successful shutdown', child => exerciseWireWrapper(child, 'success', 0, true));
  for (const stage of ['success', 'body']) {
    for (const mask of [1, 2, 3]) await t.test(`${stage} cleanup=${mask}`, child => exerciseWireWrapper(child, stage, mask));
  }
});
