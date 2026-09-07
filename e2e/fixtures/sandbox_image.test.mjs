import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {startCalcFixture} from './mcp_calc_fixture.mjs';
import {assertContainerFixtureReachable} from './sandbox_image.mjs';

test('fixture route admission uses exact substrate and never converts transport failure to model evidence', async () => {
  // C1 Docker/Podman/K8s; C2 curl has HTTP response/no response/nonzero exit;
  // C3 endpoint has credentials or invalid scheme; C4 exact test object cleanup.
  // N1 C1 x response(200/401/503) -> reachable, not model success;
  // N2 C1 x 000/exit28 -> reject before inference; N3 C3 -> no child launched;
  // N4 all launched outcomes -> remove only the generated route helper.
  // This executable CLI fixture checks process/argv/error handling. Real
  // network reachability is independently required by the service launcher.
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'cowork-route-test-'));
  const previous = {path:process.env.PATH, log:process.env.ROUTE_TEST_LOG, mode:process.env.ROUTE_TEST_MODE};
  try {
    const log = path.join(root, 'calls');
    for (const name of ['docker', 'podman', 'kubectl']) {
      await fs.writeFile(path.join(root, name), `#!${process.execPath}
` +
        `const fs=require('node:fs');fs.appendFileSync(process.env.ROUTE_TEST_LOG,JSON.stringify(process.argv.slice(2))+'\\n');` +
        `if(process.argv.includes('--write-out')){process.stdout.write('CLI notice\\nAWAKEN_FIXTURE_HTTP='+(process.env.ROUTE_TEST_MODE==='fail'?'000':process.env.ROUTE_TEST_MODE));process.exitCode=process.env.ROUTE_TEST_MODE==='fail'?28:0;}`, {mode:0o700});
    }
    process.env.PATH = root + path.delimiter + previous.path;
    process.env.ROUTE_TEST_LOG = log;
    for (const engine of ['docker','podman','k8s']) {
      for (const mode of ['200','401','503','000','fail']) {
        await fs.writeFile(log, ''); process.env.ROUTE_TEST_MODE = mode;
        const probe = assertContainerFixtureReachable({engine,image:'pinned-test-image',url:'http://192.0.2.1:30000/v1',kubeconfig:'/test/config',namespace:'isolated'});
        if (['000','fail'].includes(mode)) await assert.rejects(probe, /network preflight failed/);
        else await probe;
        const [run, cleanup] = (await fs.readFile(log,'utf8')).trim().split('\n').map(JSON.parse);
        const name = run.find(arg => arg.startsWith('cowork-route-'));
        assert.ok(name); assert.ok(cleanup.includes(name));
        assert.ok(run.includes('pinned-test-image'));
        assert.ok(run.includes('--head'), 'MCP SSE body must not be awaited');
        assert.ok(!run.includes('--privileged')); assert.ok(!run.includes('--network=host'));
      }
    }
    await fs.writeFile(log, '');
    for (const url of ['file:///tmp/no', 'http://user:secret@127.0.0.1']) {
      await assert.rejects(assertContainerFixtureReachable({engine:'docker',image:'unused',url}), /credential-free/);
    }
    assert.equal(await fs.readFile(log,'utf8'), '');
  } finally {
    process.env.PATH = previous.path;
    for (const [key, value] of [['ROUTE_TEST_LOG',previous.log],['ROUTE_TEST_MODE',previous.mode]]) {
      if(value === undefined) delete process.env[key]; else process.env[key] = value;
    }
    await fs.rm(root,{recursive:true,force:true});
  }
});

test('MCP route probes finish without creating a request or opening an SSE stream', async () => {
  // N5 HEAD x root/token endpoints -> bounded 405 with zero protocol effects;
  // ordinary authenticated POST/GET behavior remains owned by MCP E2Es.
  const fixture = await startCalcFixture('test-route-only', {allowAnonymous:true});
  try {
    for(const url of [fixture.url, fixture.tokenUrl]) {
      const response = await fetch(url,{method:'HEAD',signal:AbortSignal.timeout(2_000)});
      assert.equal(response.status,405); assert.equal(await response.text(),'');
    }
    assert.deepEqual(fixture.calls,[]); assert.deepEqual(fixture.grants,[]);
    assert.equal(fixture.unauthorized,0);
  } finally { await fixture.close(); }
});
