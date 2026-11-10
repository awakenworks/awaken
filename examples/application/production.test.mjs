// Production application-example acceptance. Fixture helpers arrange the
// deployment; the exact example programs remain the application under test.
import assert from 'node:assert/strict';
import { spawn, execFileSync } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import Anthropic from '@anthropic-ai/sdk';
import { chromium, expect } from '@playwright/test';
import {
  availablePort, spawnProduction, waitForPort, stopServer,
  provisionProviderTestModel, initializeE2EInstallation,
} from '../../e2e/harness.mjs';
import { automatedAllInOneArgs } from '../../e2e/awaken_cli_args.mjs';
import { startFakeAnthropic } from '../../e2e/fixtures/fake_anthropic_fixture.mjs';

const root = resolve(import.meta.dirname, '../..');
const data = await mkdtemp(resolve(root, 'target/application-example-'));
const port = await availablePort(39170);
const baseURL = `http://127.0.0.1:${port}`;
const live = process.env.AWAKEN_EXAMPLE_LIVE === '1';
if (live && !process.env.DEEPSEEK_API_KEY) throw new Error('Live acceptance requires DEEPSEEK_API_KEY');
const environment = Object.fromEntries(Object.entries(process.env).filter(([name]) => !name.endsWith('_API_KEY')));
const sourcePaths = ['managed.mjs', 'support.mjs', 'awaken.toml', 'server.mjs', 'src/Chat.tsx', 'src/main.tsx', 'src/style.css', 'production.test.mjs'];
const fingerprint = () => Object.fromEntries(sourcePaths.map(path => [path,
  createHash('sha256').update(readFileSync(resolve(import.meta.dirname, path))).digest('hex')]));
const sourceBefore = fingerprint();
execFileSync(process.platform === 'win32' ? 'pnpm.cmd' : 'pnpm', ['build'], {
  cwd: import.meta.dirname, env: environment, stdio: 'inherit',
});
let server, application, browser, upstream;
const scope = { betas: ['managed-agents-2026-04-01'] };
const evidence = { mode: live ? 'real-provider' : 'deterministic-provider-wire', checks: [] };

function run(args, configured) {
  const child = spawn(process.execPath, ['managed.mjs', ...args], {
    cwd: import.meta.dirname,
    env: { ...environment, AWAKEN_BASE_URL: baseURL, AWAKEN_API_KEY: 'local', AWAKEN_TIMEOUT_MS: '60000', ...configured },
  });
  let stdout = '', stderr = '';
  child.stdout.on('data', part => { stdout += part; }); child.stderr.on('data', part => { stderr += part; });
  return once(child, 'exit').then(([code]) => ({ code, stdout, stderr }));
}

async function stopApplication(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, 'exit');
  child.kill('SIGTERM');
  const timer = setTimeout(() => child.kill('SIGKILL'), 5000);
  try { await exited; } finally { clearTimeout(timer); }
}

async function verifyStopUI(browser) {
  // S1 browser streaming + refused DELETE -> retained stream and visible error;
  // S2 same stream + acknowledged 204 -> client stops, prior refusal clears.
  // This controlled protocol fixture isolates the UI's causal branch. It is
  // explicitly separate from the real production/provider checks below.
  let stops = 0;
  const fixture = createServer(async (request, response) => {
    for await (const _chunk of request) { /* consume the official client body */ }
    if (request.url === '/v1/application-access-tokens') {
      response.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify({ access_token: 'fixture-application', expires_at: '2099-01-01T00:00:00Z' })); // awaken-allow(secret): deterministic HTTP fixture value
    } else if (request.method === 'GET') {
      response.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify({ items: [] }));
    } else if (request.method === 'DELETE') {
      response.writeHead(++stops === 1 ? 403 : 204).end();
    } else {
      response.writeHead(200, { 'content-type': 'text/event-stream', 'x-vercel-ai-ui-message-stream': 'v1' });
      for (const event of [{ type: 'start', messageId: 'stop-fixture' }, { type: 'text-start', id: 'text' }, { type: 'text-delta', id: 'text', delta: 'Working' }]) {
        response.write(`data: ${JSON.stringify(event)}\n\n`);
      }
    }
  });
  fixture.listen(0, '127.0.0.1'); await once(fixture, 'listening');
  const sample = spawn(process.execPath, ['server.mjs'], {
    cwd: import.meta.dirname,
    env: { ...environment, PORT: '0', AWAKEN_BASE_URL: `http://127.0.0.1:${fixture.address().port}`, AWAKEN_SESSION_ID: 'sesn_stop_fixture', AWAKEN_API_KEY: 'fixture-backend' }, // awaken-allow(secret): deterministic HTTP fixture value
  });
  let output = ''; sample.stdout.on('data', part => { output += part; });
  const page = await browser.newPage();
  try {
    const deadline = Date.now() + 10_000;
    while (!output.includes('http://') && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 30));
    const url = output.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0]; assert.ok(url);
    await page.goto(url);
    await page.getByRole('textbox', { name: 'Message' }).fill('work');
    await page.getByRole('button', { name: 'Send', exact: true }).click();
    await expect(page.locator('[data-testid=status]')).toHaveText('streaming');
    await page.getByRole('button', { name: 'Stop', exact: true }).click();
    await expect(page.getByRole('alert')).toContainText('HTTP 403');
    await expect(page.locator('[data-testid=status]')).toHaveText('streaming');
    await page.getByRole('button', { name: 'Stop', exact: true }).click();
    await expect(page.locator('[data-testid=status]')).toHaveText('ready');
    await expect(page.getByRole('alert')).toHaveCount(0);
  } finally {
    await page.close(); await stopApplication(sample);
    fixture.closeAllConnections(); await new Promise(resolve => fixture.close(resolve));
  }
}

try {
  server = spawnProduction(data, port, { fields: { sandbox_tier: 'local' } }, environment);
  await waitForPort(port, 120_000, server);
  const nativeExecutableBefore = createHash('sha256').update(readFileSync(server.spawnfile)).digest('hex');
  // P0 run the README's configuration and existing first-install command on
  // empty owned storage, using the platform's default tier rather than the
  // portable Local tier chosen by the provider tests below.
  const coldPort = await availablePort(39171);
  const coldConfig = resolve(data, 'guide.toml');
  const supplied = readFileSync(new URL('./awaken.toml', import.meta.url), 'utf8');
  await writeFile(coldConfig, supplied.replace('"./target/application-data"', JSON.stringify(resolve(data, 'guide-data')))
    .replace('127.0.0.1:8080', `127.0.0.1:${coldPort}`));
  initializeE2EInstallation(environment, { configPath: coldConfig, binary: server.spawnfile, cwd: root });
  const cold = spawn(server.spawnfile, automatedAllInOneArgs('--config', coldConfig), { cwd: root, env: environment, stdio: ['ignore', 'ignore', 'inherit'] });
  try {
    await waitForPort(coldPort, 120_000, cold);
    const consoleResponse = await fetch(`http://127.0.0.1:${coldPort}/`);
    assert.equal(consoleResponse.status, 200);
    assert.match(consoleResponse.headers.get('content-type'), /text\/html/);
  } finally { await stopServer(cold, { requireCleanExit: true }); }
  evidence.checks.push('P0 empty installation using the documented configuration');
  const context = await fetch(`${baseURL}/v1/config/workspace-context`).then(response => response.json());
  if (!live) upstream = await startFakeAnthropic('example-fixture', { behavior: 'echo', models: ['fake-haiku'] });
  const configured = await provisionProviderTestModel(baseURL, context.workspace_id, {
    id: 'application-example', provider_id: live ? 'deepseek' : 'anthropic',
    dialect: live ? 'open_ai_chat' : 'anthropic_messages',
    base_url: live ? 'https://api.deepseek.com' : `${upstream.url}/v1/`,
    secret: live ? process.env.DEEPSEEK_API_KEY : 'example-fixture',
    model_id: live ? process.env.AWAKEN_EXAMPLE_MODEL : 'fake-haiku', timeout_secs: 60,
  });
  const client = new Anthropic({ baseURL, apiKey: 'local', maxRetries: 0 });
  evidence.model_id = configured.offering.model_id;
  const agent = await client.beta.agents.create({
    name: 'Application example', model: configured.model,
    system: 'Return the user message exactly. Do not call tools.', tools: [], ...scope,
  });
  const env = await client.beta.environments.create({ name: 'Application example', ...scope });
  const config = { AWAKEN_AGENT_ID: agent.id, AWAKEN_ENVIRONMENT_ID: env.id };

  // P1 exact published inputs + a real process -> example prints a reply and
  // natural terminal. P2 same Session + another input -> prior history retained.
  // P3 read-only reopen -> no new input; both replies remain readable.
  // SDK-only fixture tests own refusal/timeout combinations; this lane proves
  // the original Node program reaches the actual production applications.
  const first = await run(['FIRST-APPLICATION-EXAMPLE'], config);
  assert.equal(first.code, 0, first.stderr);
  const sessionId = first.stdout.match(/^session=(.+)$/m)?.[1]; assert.ok(sessionId, first.stdout);
  assert.match(first.stdout, /FIRST-APPLICATION-EXAMPLE/); assert.match(first.stdout, /status=end_turn/);
  const second = await run(['--session', sessionId, 'SECOND-APPLICATION-EXAMPLE'], config);
  assert.equal(second.code, 0, second.stderr); assert.match(second.stdout, /SECOND-APPLICATION-EXAMPLE/);
  const history = await run(['--session', sessionId, '--history'], config);
  assert.equal(history.code, 0, history.stderr);
  assert.match(history.stdout, /FIRST-APPLICATION-EXAMPLE/); assert.match(history.stdout, /SECOND-APPLICATION-EXAMPLE/);
  evidence.checks.push('P1 backend response', 'P2 same-Session second message', 'P3 committed history readback');

  application = spawn(process.execPath, ['server.mjs'], {
    cwd: import.meta.dirname,
    env: { ...environment, PORT: '0', AWAKEN_BASE_URL: baseURL, AWAKEN_API_KEY: 'local', AWAKEN_SESSION_ID: sessionId },
  });
  let output = ''; application.stdout.on('data', part => { output += part; });
  const readyDeadline = Date.now() + 10_000;
  while (!output.includes('http://') && Date.now() < readyDeadline) await new Promise(resolve => setTimeout(resolve, 30));
  const applicationURL = output.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0]; assert.ok(applicationURL);
  browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 390, height: 900 } });
  const errors = []; page.on('pageerror', error => errors.push(error.message));

  // P4 browser receives a short-lived token for that Session, reads the backend
  // transcript and sends through official AI SDK. P5 reload -> a fresh token,
  // same binding, committed history; no localStorage transcript or second Run.
  await page.goto(applicationURL);
  await expect(page.getByRole('list', { name: 'Conversation' })).toContainText('FIRST-APPLICATION-EXAMPLE');
  await page.getByRole('textbox', { name: 'Message' }).fill('BROWSER-APPLICATION-EXAMPLE');
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await expect(page.locator('[data-testid=status]')).toHaveText('ready', { timeout: 60_000 });
  await expect(page.locator('li').filter({ hasText: 'assistant' }).last()).toContainText('BROWSER-APPLICATION-EXAMPLE');
  await page.reload();
  await expect(page.getByRole('list', { name: 'Conversation' })).toContainText('BROWSER-APPLICATION-EXAMPLE');
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true);
  assert.deepEqual(errors, []);
  await page.screenshot({ path: resolve(data, 'application.png'), fullPage: true });
  const persisted = []; for await (const event of client.beta.sessions.events.list(sessionId, scope)) persisted.push(event);
  assert.equal(persisted.filter(event => event.type === 'user.message').length, 3);
  evidence.checks.push('P4 mobile browser AI SDK', 'P5 reload with same Session');
  await verifyStopUI(browser);
  evidence.fixture_checks = ['S1 refused Stop retains stream', 'S2 acknowledged Stop clears refusal'];
  if (live) {
    // P6 a normal first-use task, beyond the causal echo markers above, supplies
    // the screenshot. The output is a model answer to inspect, not customer proof.
    await client.beta.agents.update(agent.id, {
      system: 'Give an API review checklist with exactly two headings: Request shape and Failure handling. Each heading has exactly two short bullets. Maximum 100 words. No tools.', ...scope,
    });
    const useful = await run(['Review a POST /orders API accepting a customer ID and item list. What should I check before shipping?'], config);
    assert.equal(useful.code, 0, useful.stderr);
    assert.match(useful.stdout, /Request shape/); assert.match(useful.stdout, /Failure handling/);
    const usefulSession = useful.stdout.match(/^session=(.+)$/m)?.[1]; assert.ok(usefulSession);
    await stopApplication(application);
    application = spawn(process.execPath, ['server.mjs'], {
      cwd: import.meta.dirname,
      env: { ...environment, PORT: '0', AWAKEN_BASE_URL: baseURL, AWAKEN_API_KEY: 'local', AWAKEN_SESSION_ID: usefulSession },
    });
    let shown = ''; application.stdout.on('data', part => { shown += part; });
    const shownDeadline = Date.now() + 10_000;
    while (!shown.includes('http://') && Date.now() < shownDeadline) await new Promise(resolve => setTimeout(resolve, 30));
    const shownURL = shown.match(/http:\/\/127\.0\.0\.1:\d+/)?.[0]; assert.ok(shownURL);
    await page.setViewportSize({ width: 1100, height: 900 });
    await page.goto(shownURL);
    await expect(page.getByRole('list', { name: 'Conversation' })).toContainText('Failure handling');
    await page.screenshot({ path: resolve(data, 'showcase.png'), fullPage: true });
    evidence.checks.push('P6 inspectable first-use API review');
  }
  assert.deepEqual(fingerprint(), sourceBefore, 'example or oracle source changed during acceptance');
  evidence.source_sha256 = sourceBefore;
  assert.equal(createHash('sha256').update(readFileSync(server.spawnfile)).digest('hex'), nativeExecutableBefore,
    'production executable changed during acceptance');
  evidence.native_executable_sha256 = nativeExecutableBefore;
  await writeFile(resolve(data, 'evidence.json'), JSON.stringify(evidence, null, 2));
  console.log(`Application example acceptance passed (${evidence.mode}): ${evidence.checks.join(', ')}`);
  console.log(`Evidence: ${resolve(data, 'evidence.json')}`);
} finally {
  if (browser) await browser.close();
  if (application) await stopApplication(application);
  if (server) await stopServer(server, { requireCleanExit: true });
  if (upstream) await upstream.close();
}
