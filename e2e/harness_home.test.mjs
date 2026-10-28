import assert from 'node:assert/strict';
import fs from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

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
