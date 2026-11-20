// Doctor must report readiness through both its JSON and process exit code.
// Uses the portable real-process ACP peer; never installs npm or calls an LLM.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { cargoExecutable } from './cargo_binary.mjs';
import { availablePort } from './harness.mjs';
import { installHostResourceFixture } from './fixtures/trusted_host_resources.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'acp-doctor-'));
const binary = cargoExecutable({
  cwd: root, packageName: 'awaken-cli', targetName: 'awaken',
  features: ['test-support'],
  snapshotDestination: path.join(temp, 'product', process.platform === 'win32' ? 'awaken.exe' : 'awaken'),
});
const listener = net.createServer();
try {
  await new Promise((resolve, reject) => {
    listener.once('error', reject);
    listener.listen({ host: '127.0.0.1', port: 0, exclusive: true }, resolve);
  });
  const fresh = path.join(temp, 'fresh');
  const config = path.join(temp, 'doctor.toml');
  const publicBind = '127.0.0.1:' + await availablePort();
  const configure = (data, bind = publicBind) => fs.writeFileSync(config, [
    'data_dir = ' + JSON.stringify(data), 'bind = ' + JSON.stringify(bind),
    'identity_mode = "self-managed"', 'acp_clis = ["codex"]', 'sandbox_tier = "local"',
    'control_seal_key = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"',
  ].join('\n'));
  const run = (args, env = process.env) => {
    const child = spawnSync(binary, [...args, '--json', '--config', config], {
      env, encoding: 'utf8', timeout: 60_000,
    });
    assert.ifError(child.error);
    assert.ok(child.stdout.trim(), 'doctor emitted no report: ' + child.stderr);
    return { code: child.status, report: JSON.parse(child.stdout), stderr: child.stderr };
  };
  configure(fresh);
  let result = run(['doctor']);
  assert.equal(result.code, 0, result.stderr);
  assert.equal(result.report.ready, true);
  assert.equal(fs.existsSync(fresh), false, 'first-start doctor is read-only');

  configure(fresh, '127.0.0.1:' + listener.address().port);
  result = run(['doctor']);
  assert.equal(result.code, 1, 'blocked prerequisites must fail the command');
  assert.equal(result.report.ready, false);
  assert.equal(result.report.listener.status, 'blocked');
  assert.equal(fs.existsSync(fresh), false);

  const data = path.join(temp, 'prepared');
  const environment = await installHostResourceFixture(root, temp, data, 'codex');
  configure(data);
  result = run(['doctor', 'acp'], environment);
  assert.equal(result.code, 0, result.stderr);
  assert.deepEqual(result.report.acp.map((row) => row.id), ['codex']);
  assert.equal(result.report.acp[0].detected, true);
  assert.equal(result.report.acp[0].login_state, 'available');
  assert.equal(result.report.inference_checked, false);
  assert.equal(result.report.acp[0].probe, undefined, 'inventory never negotiates implicitly');

  const before = fs.readFileSync(path.join(data, 'acp-wrappers', 'codex', 'package.json'), 'utf8');
  result = run(['doctor', 'acp', '--probe'], environment);
  assert.equal(result.code, 0, JSON.stringify(result));
  assert.equal(result.report.probe_ready, true);
  assert.equal(result.report.acp[0].probe.state, 'verified');
  assert.equal(result.report.acp[0].execution_source, 'host');
  assert.equal(result.report.inference_checked, false);
  assert.equal(fs.readFileSync(path.join(data, 'acp-wrappers', 'codex', 'package.json'), 'utf8'), before);

  const absent = path.join(temp, 'no-adapter');
  configure(absent);
  result = run(['doctor', 'acp', '--probe'], environment);
  assert.equal(result.code, 1);
  assert.equal(result.report.probe_ready, false);
  assert.equal(result.report.acp[0].probe.state, 'not_prepared');
  assert.equal(fs.existsSync(absent), false, 'probe must never prepare or install adapters');
  console.log('doctor: startup, blocked listener, inventory, ACP handshake, missing adapter and read-only checks passed');
} finally {
  await new Promise((resolve) => listener.close(resolve));
  const relative = path.relative(os.tmpdir(), temp);
  assert.ok(relative.startsWith('acp-doctor-') && !relative.includes(path.sep),
    'cleanup is restricted to this task-owned temporary directory');
  fs.rmSync(temp, { recursive: true, force: true });
}
