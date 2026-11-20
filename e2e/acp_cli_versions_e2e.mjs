// Opt-in qualification of installed, real CLI versions through product doctor.
// The external case file supplies executables/packages; this test never installs
// or opens the operator's CLI home. Rust wire tests separately prove zero
// session/load and prompt dispatch on a changed or unbound private-session pin.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { AWAKEN_BIN_ENV, cargoExecutable } from './cargo_binary.mjs';
import { snapshotExecutable } from './executable_snapshot.mjs';
import { availablePort } from './harness.mjs';
import { withoutControlAuthority } from './process_environment.mjs';

assert.ok(process.env.AWAKEN_ACP_VERSION_CASES, 'AWAKEN_ACP_VERSION_CASES must name an external qualification case file');
const cases = JSON.parse(fs.readFileSync(process.env.AWAKEN_ACP_VERSION_CASES, 'utf8'));
assert.ok(Array.isArray(cases) && cases.length > 0);
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-versions-'));
try {
  const binary = cargoExecutable({ cwd: root, packageName: 'awaken-cli', targetName: 'awaken', prebuiltEnvironmentName: AWAKEN_BIN_ENV,
    snapshotDestination: path.join(temp, 'product', process.platform === 'win32' ? 'awaken.exe' : 'awaken') });
  for (const [ordinal, entry] of cases.entries()) {
    assert.ok(['codex', 'claude', 'opencode'].includes(entry.id));
    assert.ok(path.isAbsolute(entry.executable) && fs.statSync(entry.executable).isFile());
    const home = path.join(temp, String(ordinal));
    const bin = path.join(home, 'bin');
    fs.mkdirSync(bin, { recursive: true });
    snapshotExecutable(entry.executable, path.join(bin, entry.id + (process.platform === 'win32' ? '.exe' : '')));
    const data = path.join(home, 'data');
    let manifest;
    if (entry.wrapper) {
      const separator = entry.wrapper.package.lastIndexOf('@');
      assert.ok(separator > 0);
      const prefix = path.join(data, 'acp-wrappers', entry.id);
      fs.mkdirSync(prefix, { recursive: true });
      manifest = path.join(prefix, 'package.json');
      fs.writeFileSync(manifest, JSON.stringify({ dependencies: {
        [entry.wrapper.package.slice(0, separator)]: entry.wrapper.package.slice(separator + 1),
      } }));
      assert.ok(path.isAbsolute(entry.wrapper.nodeModules));
      fs.symlinkSync(entry.wrapper.nodeModules, path.join(prefix, 'node_modules'), process.platform === 'win32' ? 'junction' : 'dir');
    }
    const config = path.join(home, 'doctor.toml');
    fs.writeFileSync(config, [
      'data_dir = ' + JSON.stringify(data), 'bind = ' + JSON.stringify('127.0.0.1:' + await availablePort()),
      'identity_mode = "self-managed"', 'sandbox_tier = "local"',
      'acp_clis = ' + JSON.stringify([entry.id]),
    ].join('\n'));
    const environment = {
      ...withoutControlAuthority(process.env), HOME: home, USERPROFILE: home,
      APPDATA: path.join(home, 'roaming'), LOCALAPPDATA: path.join(home, 'local'),
      XDG_CONFIG_HOME: path.join(home, 'config'), XDG_DATA_HOME: path.join(home, 'share'),
      XDG_CACHE_HOME: path.join(home, 'cache'), XDG_STATE_HOME: path.join(home, 'state'),
      CODEX_HOME: path.join(home, 'codex'), CLAUDE_CONFIG_DIR: path.join(home, 'claude'),
      OPENCODE_CONFIG_DIR: path.join(home, 'opencode'),
      PATH: [bin, path.dirname(process.execPath), process.env.SystemRoot && path.join(process.env.SystemRoot, 'System32'), '/usr/bin', '/bin'].filter(Boolean).join(path.delimiter),
      TEMP: home, TMP: home, TMPDIR: home,
    };
    for (const name of ['NODE_OPTIONS', 'CLAUDE_CODE_OAUTH_TOKEN', 'ANTHROPIC_AUTH_TOKEN', 'CODEX_PATH', 'CLAUDE_CODE_EXECUTABLE']) delete environment[name];
    for (const directory of ['APPDATA', 'LOCALAPPDATA', 'CODEX_HOME', 'CLAUDE_CONFIG_DIR', 'OPENCODE_CONFIG_DIR']) fs.mkdirSync(environment[directory], { recursive: true });
    if (entry.apiKeyFixture) {
      assert.equal(entry.id, 'codex');
      fs.writeFileSync(path.join(environment.CODEX_HOME, 'auth.json'), JSON.stringify({
        OPENAI_API_KEY: 'awaken-capability-probe', // awaken-allow(secret): -- isolated, prompt-free capability fixture
      }));
    }
    const before = manifest && fs.readFileSync(manifest, 'utf8');
    const child = spawnSync(binary, ['doctor', 'acp', '--probe', '--json', '--config', config], {
      env: environment, encoding: 'utf8', timeout: 170_000,
    });
    assert.ifError(child.error);
    assert.ok(child.stdout.trim(), `${entry.id}: doctor exit ${child.status}: ${child.stderr}`);
    const report = JSON.parse(child.stdout);
    const ready = entry.ready !== false;
    assert.equal(child.status, ready ? 0 : 1, `${entry.id}: ${JSON.stringify(report)}`);
    assert.equal(report.probe_ready, ready);
    assert.equal(report.inference_checked, false);
    assert.deepEqual(report.acp.map(row => row.id), [entry.id]);
    const row = report.acp[0];
    assert.ok(row.version.includes(entry.detectedVersion), `${entry.id}: ${row.version}`);
    if (ready) {
      assert.equal(row.probe.state, 'verified');
      assert.ok(row.runtime.cli_version.includes(entry.actualVersion), JSON.stringify(row.runtime));
      assert.equal(row.runtime.source, entry.source);
      assert.equal(row.runtime.adapter_package, entry.wrapper?.package ?? null);
      assert.equal(row.probe.capabilities.protocol_version, '1');
    } else {
      assert.notEqual(row.probe.state, 'verified');
      assert.ok(entry.reason && row.reason_code === entry.reason, JSON.stringify(row));
    }
    if (manifest) assert.equal(fs.readFileSync(manifest, 'utf8'), before, 'doctor cannot rewrite the prepared package');
    console.log(`ACP CLI VERSION PASS: ${entry.id} ${entry.actualVersion ?? entry.detectedVersion}, ${ready ? 'live ACP v1' : 'rejected before negotiation'}`);
  }
} finally {
  const relative = path.relative(os.tmpdir(), temp);
  assert.ok(relative.startsWith('acp-versions-') && !relative.includes(path.sep));
  fs.rmSync(temp, { recursive: true, force: true });
}
