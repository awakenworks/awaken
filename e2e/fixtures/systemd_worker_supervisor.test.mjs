import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import { test } from 'node:test';
import { setTimeout as delay } from 'node:timers/promises';
import { systemdWorkerSupervisor } from './systemd_worker_supervisor.mjs';
import { runCleanupSteps } from '../harness.mjs';

test('actual user manager restarts failure, retains clean exit, and disposes its unit', async () => {
  // S1 exact live process exits nonzero -> manager (not this test) creates one
  // new PID/InvocationID. S2 exit zero -> no restart; strict live-at-stop rejects.
  // S3 live replacement receives normal service stop -> exit 0, no resurrection.
  // S4 spaces/percent/dollar/backslash/quotes cross unit + EnvironmentFile syntax
  // unchanged; S5 nonzero exit during service stop -> strict rejection, including
  // after the manager garbage-collects runtime properties. Logs survive every stop.
  // S6 stopping child calls this Node fixture's HTTP endpoint before returning;
  // synchronous systemctl stop would block that dependency and fail the receipt.
  // This suite requires the real
  // Linux user manager; absence fails explicitly rather than becoming a skip.
  for (const [code, stopCode] of [[7, 0], [0, 0], [7, 7]]) {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken systemd %$ '));
    const logFile = path.join(directory, 'worker.log');
    const script = path.join(directory, 'worker.mjs');
    let stopRequests = 0;
    const dependency = http.createServer((req, res) => { stopRequests++; res.end('ack'); });
    await new Promise(resolve => dependency.listen(0, '127.0.0.1', resolve));
    const value = 'space % dollar $FIXTURE_VALUE ${HOME} quote " backslash \\ literal `';
    fs.writeFileSync(script, `
      import fs from 'node:fs';
      console.log(JSON.stringify({pid:process.pid,value:process.env.FIXTURE_VALUE,arg:process.argv[2]}));
      process.on('SIGUSR1',()=>process.exit(${code}));
      process.on('SIGTERM',async()=>{await fetch(process.argv[3]);console.log('stop observed');process.exit(${stopCode});});
      setInterval(()=>{},1000);
    `);
    const service = systemdWorkerSupervisor({ directory, binary: process.execPath,
      args: [script, value, `http://127.0.0.1:${dependency.address().port}`],
      cwd: directory, environment: { FIXTURE_VALUE: value }, logFile });
    const until = async predicate => {
      const deadline = Date.now() + 15_000;
      while (!predicate()) { assert.ok(Date.now() < deadline, 'manager transition deadline'); await delay(50); }
    };
    let succeeded = false;
    const failures = [];
    try {
      await service.start();
      await until(() => fs.existsSync(logFile) && fs.readFileSync(logFile, 'utf8').includes('value'));
      const first = service.status();
      const logged = JSON.parse(fs.readFileSync(logFile, 'utf8').trim());
      assert.equal(logged.value, value, 'S4 exact environment');
      assert.equal(logged.arg, value, 'S4 exact argument');
      process.kill(first.pid, 'SIGUSR1');
      if (code !== 0) {
        await until(() => { const s = service.status(); return s.restarts === 1 && s.pid > 0 && s.pid !== first.pid; });
        const next = service.status();
        assert.notEqual(next.InvocationID, first.InvocationID, 'S1 fresh invocation');
        await until(() => fs.readFileSync(logFile, 'utf8').trim().split('\n').length === 2);
        if (stopCode === 0) await service.stop();
        else await assert.rejects(service.stop(), /exited successfully/);
        assert.match(fs.readFileSync(logFile, 'utf8'), /stop observed/);
        assert.equal(stopRequests, 1, 'S6 the dependent HTTP surface remained live during stop');
      } else {
        await until(() => service.status().ActiveState === 'inactive');
        assert.equal(service.status().restarts, 0, 'S2 no restart after clean completion');
        await assert.rejects(service.stop(), /must be live before cleanup/);
      }
      succeeded = true;
    } catch (error) {
      error.message += `; unit=${service.name}; status=${JSON.stringify(service.status())}; log=${fs.existsSync(logFile) ? fs.readFileSync(logFile, 'utf8') : 'absent'}; fixture=${directory}`;
      failures.push(error);
    } finally {
      await runCleanupSteps([
        () => service.dispose(),
        () => new Promise(resolve => dependency.close(resolve)),
        () => assert.equal(service.status().LoadState, 'not-found', 'owned unit is unlinked'),
      ], failures);
      if (succeeded) fs.rmSync(directory, { recursive: true });
    }
  }
});
