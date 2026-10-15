import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import {execFileSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {startCalcFixture} from './mcp_calc_fixture.mjs';
import {assertContainerFixtureReachable, ensureCanonicalSandboxImage} from './sandbox_image.mjs';

const pathVariable = Object.keys(process.env).find(name => name.toUpperCase() === 'PATH') || 'PATH';

async function installCommands(directory, commands) {
  // POSIX execs shebang files; native Windows CreateProcess needs an actual PE.
  // Reuse the ACP fixture's role dispatcher, keeping the real shell/CLI boundary
  // and the same argument/repair/cleanup assertions on both platforms.
  if (process.platform !== 'win32') {
    for (const [name, source] of Object.entries(commands)) {
      await fs.writeFile(path.join(directory, name), `#!${process.execPath}\n${source}`, {mode:0o755});
    }
    return;
  }
  const dispatch = path.join(directory, 'dispatch.cjs');
  await fs.writeFile(dispatch, "const path=require('node:path');const role=process.argv[2];process.argv.splice(1,1);require(path.join(__dirname,role+'.fixture.cjs'));\n");
  const parent = path.join(process.env.CARGO_TARGET_DIR || path.resolve('target'), 'debug', 'deps');
  await fs.mkdir(parent, {recursive:true});
  const build = await fs.mkdtemp(path.join(parent, 'awaken-native-test-tool-'));
  try {
    const compiler = process.env.RUSTC || 'rustc';
    const wrapper = process.env.RUSTC_WRAPPER;
    const output = path.join(build, 'fixture.exe');
    execFileSync(wrapper || compiler, [...(wrapper ? [compiler] : []), '--edition=2024',
      fileURLToPath(new URL('./acp_host_fixture_launcher.rs', import.meta.url)), '-o', output],
    {env:{...process.env, AWAKEN_FIXTURE_NODE:process.execPath, AWAKEN_FIXTURE_SCRIPT:dispatch}});
    for (const [name, source] of Object.entries(commands)) {
      await fs.writeFile(path.join(directory, name + '.fixture.cjs'), source);
      await fs.copyFile(output, path.join(directory, name + '.exe'));
    }
  } finally {await fs.rm(build, {recursive:true,force:true});}
}

test('canonical image admission verifies selected ACP runtimes on reuse and after build', async () => {
  // Causes: C1=image exists; C2=all base/role contract labels; C3=documented
  // Ubuntu/amd64/user/workdir/default-runtime foundation, including Ruby's
  // literal major.minor separator (backticks execute a command and yield 3.);
  // C4=complete
  // Hand/helper roles; C5=quality tools; C6=selected runtime negotiates;
  // C7=current inputs match; C8=build repairs C1-C7.
  // Rules: I1 all required checks pass => no build; I2 any required check
  // fails + C6 => build once and recheck; I3 !C5+!C6 => reject even after
  // build. Empty selection requires no ACP; Claude requires a real handshake.
  // This tests the actual shell admission owner with process-boundary fixtures;
  // real image/ACP acceptance remains the responsibility of the service E2E.
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'sandbox-admission-'));
  const previous = { ...process.env };
  try {
    for (const relative of ['deploy/images/sandbox/build.sh', 'deploy/images/sandbox/stage-binary.sh',
      'deploy/images/sandbox/Dockerfile', 'deploy/images/sandbox/Dockerfile.dockerignore',
      'deploy/images/sandbox/hermes-acp-bootstrap.py', 'deploy/images/sandbox/install-acp-runtimes.py',
      'deploy/images/sandbox/verify-acp-runtimes.py',
      'scripts/ci/_deadline.sh', 'scripts/ci/deadline.py', 'scripts/ci/_windows_job.py',
      'e2e/cargo_binary.mjs', 'e2e/executable_snapshot.mjs']) {
      const destination = path.join(root, relative);
      await fs.mkdir(path.dirname(destination), { recursive:true });
      await fs.copyFile(new URL('../../' + relative, import.meta.url), destination);
    }
    const bin = path.join(root, 'bin'); await fs.mkdir(bin);
    const calls = path.join(root, 'calls'); const state = path.join(root, 'built');
    const executable = path.join(bin, 'sandbox');
    await fs.writeFile(executable, '#!/bin/sh\nexit 0\n', { mode:0o755 });
    await installCommands(bin, {cargo:`
const args=process.argv.slice(2);
if(args[0]==='build') console.log(JSON.stringify({reason:'compiler-artifact',target:{name:'awaken-sandbox',kind:['bin']},executable:process.env.ADMISSION_BINARY}));
else if(args[0]==='metadata') console.log(JSON.stringify({build_directory:process.env.ADMISSION_CACHE}));
else console.log('{}');
`, docker:`
const fs=require('node:fs'),crypto=require('node:crypto'),args=process.argv.slice(2),mode=process.env.ADMISSION_MODE;
fs.appendFileSync(process.env.ADMISSION_CALLS,JSON.stringify(args)+'\\n');
const repaired=fs.existsSync(process.env.ADMISSION_STATE);
const baseProbe=args.includes('/bin/sh')&&args.at(-1)?.includes('RUBY_VERSION');
if(baseProbe&&!args.at(-1).includes('RUBY_VERSION.split(".")[0,2].join(".")'))process.exit(1);
if(args[0]==='build'){fs.writeFileSync(process.env.ADMISSION_STATE,'built');process.exit(0);}
if(args[0]==='image'){
 if(!repaired&&mode==='absent')process.exit(1);
 if(args.includes('--format')){
  const format=args[args.indexOf('--format')+1];
  if(format.includes('build-input-fingerprint'))console.log(!repaired&&mode==='current'?'0'.repeat(64):process.env.ADMISSION_FINGERPRINT);
  else if(!repaired&&mode==='label')console.log('old');
  else if(format.includes('environment-packages'))console.log('3');
  else if(format.includes('managed-agents-base-contract'))console.log('ubuntu-24.04-amd64-v1');
  else if(format.includes('sandbox-role-contract'))console.log('2');
 }
}else if(args.includes('sha256sum')){
 const target=args.at(-1),bytes=target.includes('acp-runtimes')?Buffer.from('{}\\n'):fs.readFileSync(process.env.ADMISSION_BINARY);
 const digest=!repaired&&mode==='current'?'0'.repeat(64):crypto.createHash('sha256').update(bytes).digest('hex');
 console.log(digest+'  '+target);
}else if(args.includes('/usr/local/bin/awaken-verify-acp-runtimes')){
 if(mode==='unrepairable'||(!repaired&&mode==='runtime'))process.exit(1);
}else if(args.includes('definitely-not-a-role')){
 const roles=['acp','hand','git-credential','control-forwarder-ready','repository-publish-noreplace','read-tree-nofollow','channel-listen','channel-connect','scoped-exec','repository-effect'];
 if(repaired||mode!=='roles')roles.push('control-forwarder');
 console.error('unknown role (expected: '+roles.join(' | ')+')');process.exit(1);
}else if(!repaired&&((mode==='base'&&args.includes('/bin/sh'))||(mode==='hand'&&args.includes('hand'))||(mode==='publish'&&args.includes('repository-publish-noreplace'))||(mode==='reader'&&args.includes('read-tree-nofollow'))||(mode==='tools'&&args.includes('/bin/sh'))))process.exit(1);
`});
    Object.assign(process.env, { [pathVariable]:bin + path.delimiter + previous[pathVariable],
      ADMISSION_CALLS:calls, ADMISSION_STATE:state, ADMISSION_BINARY:executable, ADMISSION_CACHE:path.join(root, 'cache'),
      AWAKEN_SANDBOX_RUST_TOOLCHAIN_IMAGE:'verified-toolchain-seed' });
    const staticInputs = ['Dockerfile', 'Dockerfile.dockerignore', 'hermes-acp-bootstrap.py',
      'install-acp-runtimes.py', 'verify-acp-runtimes.py'];
    const hash = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
    const inputHashes = await Promise.all(staticInputs.map(name => fs.readFile(path.join(root, 'deploy/images/sandbox', name)).then(hash)));
    inputHashes.push(hash(await fs.readFile(executable)), hash(Buffer.from('{}\n')));
    for (const [runtimeIds, mode] of [['','ok'], ['claude','ok'], ['all','ok'], ['claude,codex','ok'], ['', 'runtime'], ['', 'current'],
      ...['absent','label','base','roles','hand','publish','reader','tools','runtime','unrepairable'].map(mode=>['claude', mode])]) {
      await fs.writeFile(calls, ''); await fs.rm(state, { force:true });
      process.env.ADMISSION_MODE = mode;
      process.env.ADMISSION_FINGERPRINT = hash(Buffer.from(`runtime_ids=${runtimeIds}\n${inputHashes.join('\n')}\n`));
      const run = () => ensureCanonicalSandboxImage({engine:'docker',image:'exact-fixture',repoRoot:root,runtimeIds});
      if (mode === 'unrepairable') assert.throws(run); else {
        try { run(); } catch (error) { error.message = `${runtimeIds}/${mode}: ${error.message}`; throw error; }
      }
      const commands = (await fs.readFile(calls,'utf8')).trim().split('\n').map(JSON.parse);
      const mustBuild = mode !== 'ok' && !(runtimeIds === '' && mode === 'runtime');
      const builds = commands.filter(args=>args[0] === 'build');
      assert.equal(builds.length, Number(mustBuild), `${runtimeIds}/${mode}`);
      for (const args of builds) {
        const toolchain = args.indexOf('RUST_TOOLCHAIN_IMAGE=verified-toolchain-seed');
        assert.ok(toolchain > 0 && args[toolchain - 1] === '--build-arg', 'toolchain seed is one data-only build argument');
        const generated = args.indexOf('--build-context');
        assert.match(args[generated + 1], /^awaken_generated=.+/, 'current-source artifacts use one isolated named context');
        const fingerprint = args.find(arg => arg.startsWith('AWAKEN_BUILD_INPUT_FINGERPRINT='));
        assert.equal(fingerprint, `AWAKEN_BUILD_INPUT_FINGERPRINT=${process.env.ADMISSION_FINGERPRINT}`);
        assert.equal(args.at(-1), 'deploy/images/sandbox', 'unrelated repository bytes never enter the Sandbox context');
      }
      const probes = commands.filter(args=>args.includes('/usr/local/bin/awaken-verify-acp-runtimes'));
      assert.equal(probes.length, runtimeIds ? (['runtime','unrepairable'].includes(mode) ? 2 : 1) : 0);
      for (const args of probes) assert.deepEqual(args.slice(args.indexOf('exact-fixture') + 1),
        runtimeIds === 'all' ? [] : runtimeIds.split(','));
    }

    // Immutable-coordinate graph: D1 digest reference; D2 complete base/role/
    // runtime acceptance; D3 exact current binary, contract and fingerprint.
    // I8 D1+D2+D3 -> reuse unchanged. I9 D1+(!D2 or !D3) -> refuse before
    // Docker build; a digest cannot name new bytes. The earlier mutable rows
    // retain the repair effect. Each failed acceptance cause is selected here
    // independently, while the current row covers stale source inputs.
    const immutableImage = `registry.test/awaken-sandbox@sha256:${'a'.repeat(64)}`;
    for (const mode of ['ok', 'absent', 'label', 'base', 'roles', 'hand', 'publish',
      'reader', 'tools', 'runtime', 'unrepairable', 'current']) {
      await fs.writeFile(calls, ''); await fs.rm(state, { force:true });
      process.env.ADMISSION_MODE = mode;
      process.env.ADMISSION_FINGERPRINT = hash(Buffer.from(`runtime_ids=claude\n${inputHashes.join('\n')}\n`));
      const run = () => ensureCanonicalSandboxImage({engine:'docker',image:immutableImage,repoRoot:root,runtimeIds:'claude'});
      if (mode === 'ok') run(); else assert.throws(run, `immutable/${mode}`);
      const commands = (await fs.readFile(calls, 'utf8')).trim().split('\n').map(JSON.parse);
      assert.equal(commands.filter(args => args[0] === 'build').length, 0, `immutable/${mode} must never rebuild`);
      assert.equal(await fs.stat(state).then(() => true, () => false), false, 'immutable admission never replaces bytes');
    }

    // Toolchain-seed decision table: C1 an existing image passes every full
    // admission check; C2 only its current-source fingerprint is stale; C3 no
    // explicit seed is configured. Effect E1 rebuild once with that accepted
    // image as RUST_TOOLCHAIN_IMAGE. Mutants that use the unaccepted default or
    // override an explicit seed are covered by this row and the loop above.
    delete process.env.AWAKEN_SANDBOX_RUST_TOOLCHAIN_IMAGE;
    await fs.writeFile(calls, ''); await fs.rm(state, { force:true });
    process.env.ADMISSION_MODE = 'current';
    process.env.ADMISSION_FINGERPRINT = hash(Buffer.from(`runtime_ids=\n${inputHashes.join('\n')}\n`));
    ensureCanonicalSandboxImage({engine:'docker',image:'exact-fixture',repoRoot:root,runtimeIds:''});
    const seedCommands = (await fs.readFile(calls,'utf8')).trim().split('\n').map(JSON.parse);
    const [seedBuild] = seedCommands.filter(args => args[0] === 'build');
    assert.ok(seedBuild, 'stale current inputs rebuild exactly once');
    const seed = seedBuild.indexOf('RUST_TOOLCHAIN_IMAGE=exact-fixture');
    assert.ok(seed > 0 && seedBuild[seed - 1] === '--build-arg',
      'the fully accepted prior image owns the offline toolchain seed');
  } finally {
    for (const key of Object.keys(process.env)) if (!(key in previous)) delete process.env[key];
    Object.assign(process.env, previous);
    await fs.rm(root, {recursive:true,force:true});
  }
});

test('fixture route admission uses exact substrate and never converts transport failure to model evidence', async () => {
  // C1 Docker/Podman/K8s; C2 curl has HTTP response/no response/nonzero exit;
  // C3 endpoint has credentials or invalid scheme; C4 exact test object cleanup.
  // N1 C1 x response(200/401/503) -> reachable, not model success;
  // N2 C1 x 000/exit28 -> reject before inference; N3 C3 -> no child launched;
  // N4 all launched outcomes -> remove only the generated route helper.
  // This executable CLI fixture checks process/argv/error handling. Real
  // network reachability is independently required by the service launcher.
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'cowork-route-test-'));
  const previous = {path:process.env[pathVariable], log:process.env.ROUTE_TEST_LOG, mode:process.env.ROUTE_TEST_MODE};
  try {
    const log = path.join(root, 'calls');
    await installCommands(root, Object.fromEntries(['docker', 'podman', 'kubectl'].map(name => [name,
        `const fs=require('node:fs');fs.appendFileSync(process.env.ROUTE_TEST_LOG,JSON.stringify(process.argv.slice(2))+'\\n');` +
        `if(process.argv.includes('--write-out')){process.stdout.write('CLI notice\\nAWAKEN_FIXTURE_HTTP='+(process.env.ROUTE_TEST_MODE==='fail'?'000':process.env.ROUTE_TEST_MODE));process.exitCode=process.env.ROUTE_TEST_MODE==='fail'?28:0;}`])));
    process.env[pathVariable] = root + path.delimiter + previous.path;
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
    process.env[pathVariable] = previous.path;
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
