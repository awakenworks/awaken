import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { randomUUID } from 'node:crypto';
import { execFile, execFileSync } from 'node:child_process';
import { promisify } from 'node:util';
import { runCleanupSteps } from '../harness.mjs';

// Deployment adapter only. systemd owns restart; Worker/Registry/Dispatch keep
// their existing authority. No timer here launches a replacement process.
export function systemdWorkerSupervisor({ directory, binary, args, cwd, environment, logFile }) {
  assert.equal(process.platform, 'linux', 'systemd qualification requires Linux');
  const ctl = (...args) => execFileSync('systemctl', ['--user', ...args], {
    encoding: 'utf8', timeout: 100_000, stdio: ['ignore', 'pipe', 'pipe'],
  });
  // In particular stop must yield: this fixture's TLS proxy remains live while
  // the Worker drains and sends its final authenticated receipts through it.
  const mutate = (...args) => promisify(execFile)('systemctl', ['--user', ...args], {
    encoding: 'utf8', timeout: 100_000,
  });
  ctl('show', '--property=Version'); // Fail before creating any fixture files.
  const name = `awaken-worker-e2e-${randomUUID()}.service`;
  const unitFile = path.join(directory, name);
  const environmentFile = path.join(directory, `${name}.env`);
  const receiptFile = path.join(directory, `${name}.exits.jsonl`);
  const quote = value => '"' + String(value).replaceAll('\\', '\\\\').replaceAll('"', '\\"') + '"';
  const unitQuote = value => quote(value).replaceAll('%', '%%');
  const unitPath = value => {
    assert.ok(!/[\r\n\0]/.test(value), 'unit paths must be single-line');
    return value.replaceAll('%', '%%');
  };
  const argvQuote = value => unitQuote(value).replaceAll('$', () => '$$');
  for (const [key, value] of Object.entries(environment)) {
    assert.match(key, /^[A-Za-z_][A-Za-z0-9_]*$/, 'environment variable name');
    assert.ok(!/[\r\n\0]/.test(value), 'fixture environment values must be single-line');
    assert.ok(!['INVOCATION_ID', 'SERVICE_RESULT', 'EXIT_CODE', 'EXIT_STATUS'].includes(key),
      'systemd terminal metadata is manager-owned');
  }
  // EnvironmentFile overrides selected values. UnsetEnvironment prevents the
  // user manager's unrelated credentials/config from filling omitted variables.
  const inherited = ctl('show-environment').split('\n').map(row => row.split('=', 1)[0])
    .filter(key => /^[A-Za-z_][A-Za-z0-9_]*$/.test(key) && !Object.hasOwn(environment, key));
  // Inactive units can be garbage-collected before a subsequent `show` call.
  // Observe the manager's terminal facts in its own ExecStopPost boundary, keyed
  // by InvocationID; never interpret a reloaded unit's default status 0 as proof.
  const observeExit = "require('node:fs').appendFileSync(process.argv[1],JSON.stringify({invocation:process.env.INVOCATION_ID,result:process.env.SERVICE_RESULT,code:process.env.EXIT_CODE,status:process.env.EXIT_STATUS})+'\\n',{mode:0o600})";
  fs.writeFileSync(environmentFile, Object.entries(environment)
    .map(([key, value]) => `${key}=${quote(value)}`).join('\n') + '\n', { mode: 0o600 });
  fs.writeFileSync(unitFile, [
    '[Unit]', 'Description=Awaken isolated Worker E2E',
    'StartLimitIntervalSec=60', 'StartLimitBurst=3', '[Service]', 'Type=exec',
    `ExecStart=${[binary, ...args].map(argvQuote).join(' ')}`,
    `ExecStopPost=${[process.execPath, '-e', observeExit, receiptFile].map(argvQuote).join(' ')}`,
    `WorkingDirectory=${unitPath(cwd)}`, `EnvironmentFile=${unitPath(environmentFile)}`,
    `UnsetEnvironment=${inherited.join(' ')}`,
    `StandardOutput=append:${unitPath(logFile)}`, 'StandardError=inherit',
    'Restart=on-failure', 'RestartSec=1s', 'KillSignal=SIGTERM',
    'KillMode=mixed', 'TimeoutStopSec=90s', '',
  ].join('\n'), { mode: 0o600 });
  let linked = false;
  const exits = () => fs.existsSync(receiptFile)
    ? fs.readFileSync(receiptFile, 'utf8').trim().split('\n').filter(Boolean).map(row => JSON.parse(row)) : [];
  const status = () => {
    const values = Object.fromEntries(ctl('show', name,
      '--property=LoadState,ActiveState,SubState,MainPID,NRestarts,InvocationID,ExecMainCode,ExecMainStatus,Result')
      .trim().split('\n').map(row => [row.slice(0, row.indexOf('=')), row.slice(row.indexOf('=') + 1)]));
    return { ...values, pid: Number(values.MainPID), restarts: Number(values.NRestarts) };
  };
  return {
    name, status, exits,
    async start() {
      await mutate('link', unitFile);
      linked = true;
      await mutate('daemon-reload');
      await mutate('start', name);
      const current = status();
      assert.equal(current.ActiveState, 'active');
      assert.ok(current.pid > 0);
      assert.equal(current.restarts, 0, 'startup must not hide a failed first incarnation');
    },
    async stop() {
      if (!linked) return;
      const before = status();
      await mutate('stop', name);
      const after = status();
      {
        assert.equal(before.SubState, 'running', 'supervised Worker must be live before cleanup');
        const receipts = exits().filter(row => row.invocation === before.InvocationID);
        assert.equal(receipts.length, 1, 'one exact manager terminal observation');
        assert.equal(receipts[0].code, 'exited', 'supervised Worker exited normally');
        assert.equal(receipts[0].status, '0', 'supervised Worker exited successfully');
        assert.equal(receipts[0].result, 'success');
      }
      assert.equal(after.pid, 0);
      return after;
    },
    async dispose() {
      if (!linked) return;
      // Always stop before unlinking; exact terminal observations remain in the
      // receipt file through cleanup. Only this random owned unit is used.
      await runCleanupSteps([
        () => status().ActiveState !== 'inactive' ? mutate('stop', name) : undefined,
        () => mutate('disable', name),
        () => status().ActiveState === 'failed' ? mutate('reset-failed', name) : undefined,
        () => mutate('daemon-reload'),
      ]);
      linked = false;
    },
  };
}
