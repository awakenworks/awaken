import assert from 'node:assert/strict';
import { test } from 'node:test';
import { mkdtemp, mkdir, writeFile, readFile, rm, cp } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

test('README projection detects source drift and repairs only the generated block', async () => {
  // Causes: C1 one valid marker pair; C2 executable source changed; C3 missing
  // or duplicated markers; C4 unsupported CLI mode. Effects: E1 --check is
  // read-only, rejects stale source; E2 generation preserves surrounding prose;
  // E3 malformed inputs/modes refuse. Rules D1 C1+C2 -> E1 then E2; D2 C3/C4
  // -> E3. Mutation of the canonical source, not a mirrored expected snippet,
  // is the negative oracle. Every file here belongs to a temporary fixture.
  const root = await mkdtemp(resolve(tmpdir(), 'awaken-example-docs-'));
  try {
    await mkdir(resolve(root, 'scripts')); await mkdir(resolve(root, 'examples/application'), { recursive: true });
    await cp(new URL('../../scripts/generate-example-docs.mjs', import.meta.url), resolve(root, 'scripts/generate-example-docs.mjs'));
    const prose = 'before\n<!-- managed-example:start -->\n<!-- managed-example:end -->\nafter\n';
    await writeFile(resolve(root, 'README.md'), prose);
    const source = resolve(root, 'examples/application/managed.mjs');
    await writeFile(source, 'console.log("first");\n');
    const run = (...args) => spawnSync(process.execPath, [resolve(root, 'scripts/generate-example-docs.mjs'), ...args], { encoding: 'utf8' });
    assert.equal(run('--check').status, 1);
    assert.equal(await readFile(resolve(root, 'README.md'), 'utf8'), prose);
    assert.equal(run().status, 0); assert.equal(run('--check').status, 0);
    await writeFile(source, 'console.log("changed");\n');
    assert.equal(run('--check').status, 1); assert.equal(run().status, 0);
    const generated = await readFile(resolve(root, 'README.md'), 'utf8');
    assert.ok(generated.startsWith('before\n')); assert.ok(generated.endsWith('\nafter\n'));
    assert.ok(generated.includes('console.log("changed")'));
    await writeFile(resolve(root, 'README.md'), 'no markers');
    assert.notEqual(run().status, 0); assert.equal(run('--unknown').status, 2);
    await writeFile(resolve(root, 'README.md'), '<!-- managed-example:end --><!-- managed-example:start -->');
    assert.notEqual(run('--check').status, 0);
  } finally { await rm(root, { recursive: true, force: true }); }
});
