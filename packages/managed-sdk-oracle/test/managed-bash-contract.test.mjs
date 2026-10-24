import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import test from 'node:test';

import { resolveSdkPackage } from '../src/package-source.mjs';
import { MANAGED_HELPER_PACKAGES } from './support/managed-helper-packages.mjs';

if (process.platform === 'win32') {
  const distribution = process.env.AWAKEN_TEST_WSL_DISTRO;
  // Without a selected WSL runtime the Bash contracts are reported as skipped, not passed.
  const skip = !distribution
    && 'Bash helper contracts need AWAKEN_TEST_WSL_DISTRO (WSL Linux, Node.js >=22); not executed';
  test('all configured official Bash helper contracts execute on the configured WSL runtime', { skip }, (t) => {
    const wsl = (...args) => {
      const result = spawnSync('wsl.exe', ['--distribution', distribution, '--exec', ...args], {
        encoding: 'utf8', windowsHide: true, timeout: 120_000,
      });
      if (result.error) throw result.error;
      assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
      return result.stdout;
    };
    const file = wsl('wslpath', '-u', resolve(import.meta.filename)).trim();
    // Do not delegate to a Windows node.exe inherited through WSL's PATH.
    wsl('node', '--input-type=module', '-e',
      "if(process.platform!=='linux'||Number(process.versions.node.split('.')[0])<22)process.exit(1)");
    const output = wsl('node', '--test', '--test-reporter=tap', file);
    for (const [field, count] of [
      ['tests', MANAGED_HELPER_PACKAGES.length], ['pass', MANAGED_HELPER_PACKAGES.length],
      ['fail', 0], ['skipped', 0], ['cancelled', 0],
    ]) assert.match(output, new RegExp(`^# ${field} ${count}\\r?$`, 'mu'));
    t.diagnostic(`WSL ${distribution}:\n${output}`);
  });
} else {
  for (const alias of MANAGED_HELPER_PACKAGES) {
    test(`${alias}: BashSession and betaBashTool retain executable semantics`, async () => {
      const sdk = resolveSdkPackage(alias);
      const module = await import(pathToFileURL(join(sdk.root, 'tools/agent-toolset/node.mjs')).href);
      const workdir = mkdtempSync(join(tmpdir(), 'managed-bash-contract-'));
      let shell;
      let bash;
      try {
        shell = new module.BashSession(workdir, { PATH: process.env.PATH ?? '' });
        assert.deepEqual(await shell.exec('printf first'), { output: 'first', exitCode: 0 });
        await shell.exec('export MANAGED_HELPER_STATE=preserved');
        assert.deepEqual(await shell.exec('printf %s "$MANAGED_HELPER_STATE"'),
          { output: 'preserved', exitCode: 0 }, 'BashSession preserves shell state');
        shell.close();
        assert.equal(shell.closed, true);
        await assert.rejects(() => shell.exec('true'), /terminated/u);
        bash = module.betaBashTool({ workdir });
        assert.equal(await bash.run({ command: 'printf helper' }), 'helper');
        await assert.rejects(() => bash.run({ command: '' }), /command is required/u);
      } finally {
        shell?.close();
        bash?.close();
        rmSync(workdir, { recursive: true, force: true });
      }
    });
  }
}
