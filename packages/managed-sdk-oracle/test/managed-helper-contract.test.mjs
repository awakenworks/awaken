import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import test from 'node:test';

import { managedExportFingerprint } from '../src/extract-exports.mjs';
import { resolveSdkPackage } from '../src/package-source.mjs';

const PACKAGE_ALIASES = Object.freeze([
  '@anthropic-ai/sdk-oldest',
  '@anthropic-ai/sdk-user-profiles-legacy',
  '@anthropic-ai/sdk-current',
  '@anthropic-ai/sdk-candidate',
]);
const SCOPE = JSON.parse(readFileSync(resolve(import.meta.dirname, '../config/scope.json'), 'utf8'));

// Causal graph: C1 an official Managed helper is statically exported from one
// reviewed entrypoint; C2 ESM exposes that same symbol; C3 the symbol belongs
// to exactly one explicit semantic group below; C4 that group's executable
// contract runs against every exact SDK root. Effects: E1 source/runtime
// export agreement, E2 no new helper inherits an unrelated file's fingerprint,
// E3 constants, predicates, factories, stateful classes, filesystem tools,
// schemas and accumulators retain developer-visible behavior. Decision table:
// C1+!C2 => import failure; C1+C2+!C3 => unowned-export failure;
// C1+C2+C3+!C4 => behavior failure; all causes true => compatible helper.
// Metamorphic checks mutate status classes, paths, Zod inputs, event ordering,
// shell state and memory lifecycle so a type-correct but behaviorally different
// implementation cannot satisfy the contract with one happy-path value.
const OWNED_EXPORTS = Object.freeze({
  'helpers/beta/zod.mjs': Object.freeze([
    'betaZodOutputFormat',
    'betaZodTool',
  ]),
  'lib/environments/index.mjs': Object.freeze([
    'backoff',
    'DEFAULT_MAX_IDLE_MS',
    'EnvironmentWorker',
    'is4xx',
    'isFatal4xx',
    'isStatus',
    'jitter',
    'MANAGED_AGENTS_BETA',
    'POLL_BLOCK_MS',
    'SessionToolRunner',
    'WorkPoller',
  ]),
  'lib/sessions/accumulate.mjs': Object.freeze([
    'accumulateManagedAgentsEvent',
  ]),
  'tools/agent-toolset/node.mjs': Object.freeze([
    'BashSession',
    'BashTimeoutError',
    'betaAgentToolset20260401',
    'betaBashTool',
    'betaEditTool',
    'betaGlobTool',
    'betaGrepTool',
    'betaReadTool',
    'betaWriteTool',
    'DEFAULT_MEMORY_SYNC_INTERVAL_MS',
    'extractSkillArchive',
    'MARKER_PATH',
    'MEMORY_FLUSH_TIMEOUT_MS',
    'MIN_MEMORY_SYNC_INTERVAL_MS',
    'resolvePath',
    'resolveSkillVersion',
    'SessionMemoryError',
    'SessionMemoryStores',
    'setupSkills',
  ]),
});

function moduleURL(root, entrypoint) {
  return pathToFileURL(resolve(root, entrypoint)).href;
}

async function importedEntrypoints(root, evidence) {
  const modules = new Map();
  for (const entrypoint of new Set(evidence.exports.map(({ entrypoint }) => entrypoint))) {
    modules.set(entrypoint, await import(moduleURL(root, entrypoint)));
  }
  return modules;
}

function assertExactOwnedSurface(evidence, modules, label) {
  const expected = evidence.exports.map(({ id }) => id).sort();
  const imported = [...modules].flatMap(([entrypoint, module]) => Object.keys(module)
    .map((name) => `${entrypoint}#${name}`)).sort();
  assert.deepEqual(imported, expected, `${label}: static and executable exports`);

  const owned = evidence.exports.map(({ entrypoint, name }) => {
    assert.ok(
      OWNED_EXPORTS[entrypoint]?.includes(name),
      `${label}: ${entrypoint}#${name} has no explicit behavior owner`,
    );
    return `${entrypoint}#${name}`;
  }).sort();
  assert.deepEqual(owned, expected, `${label}: every executable export is owned once`);
}

async function assertZodHelpers(module, root, label) {
  const requireFromSdk = createRequire(resolve(root, 'package.json'));
  const { z } = await import(pathToFileURL(requireFromSdk.resolve('zod')).href);
  const format = module.betaZodOutputFormat(z.object({ answer: z.number().int() }));
  assert.equal(format.type, 'json_schema', `${label}: Zod output kind`);
  assert.equal(format.schema.type, 'object', `${label}: Zod output schema`);
  assert.deepEqual(format.parse('{"answer":42}'), { answer: 42 }, `${label}: Zod output parse`);
  assert.throws(() => format.parse('{"answer":"42"}'), /Failed to parse structured output/u);
  assert.throws(() => format.parse('{invalid'), SyntaxError, `${label}: malformed JSON remains distinct`);

  let received;
  const tool = module.betaZodTool({
    name: 'typed_helper',
    description: 'typed helper contract',
    inputSchema: z.object({ value: z.number().int() }),
    run: async (input) => { received = input; return input.value + 1; },
  });
  assert.equal(tool.type, 'custom', `${label}: Zod tool kind`);
  assert.equal(tool.input_schema.type, 'object', `${label}: Zod tool schema`);
  assert.deepEqual(tool.parse({ value: 7 }), { value: 7 }, `${label}: Zod tool parse`);
  assert.throws(() => tool.parse({ value: '7' }), /invalid_type/u);
  return tool.run({ value: 7 }).then((value) => {
    assert.equal(value, 8, `${label}: Zod tool preserves run callback`);
    assert.deepEqual(received, { value: 7 });
  });
}

function apiError(Anthropic, status) {
  return Anthropic.APIError.generate(
    status,
    { error: { type: 'invalid_request_error', message: `status ${status}` } },
    undefined,
    new Headers(),
  );
}

function assertEnvironmentHelpers(module, Anthropic, label) {
  assert.equal(module.MANAGED_AGENTS_BETA, 'managed-agents-2026-04-01');
  assert.equal(module.POLL_BLOCK_MS, 999);
  assert.equal(module.DEFAULT_MAX_IDLE_MS, 60_000);
  assert.deepEqual([0, 1, 2, 5, 6, 20].map(module.backoff), [1_000, 2_000, 4_000, 32_000, 60_000, 60_000]);
  for (let sample = 0; sample < 32; sample += 1) {
    const value = module.jitter(10, 20);
    assert.ok(value >= 10 && value < 20, `${label}: jitter range`);
  }

  const statuses = new Map([400, 408, 409, 429, 500].map((status) => [status, apiError(Anthropic, status)]));
  assert.equal(module.isStatus(statuses.get(409), 409), true);
  assert.equal(module.isStatus(statuses.get(409), 400), false);
  assert.equal(module.isStatus({ status: 409 }, 409), false, `${label}: nominal APIError identity`);
  assert.equal(module.is4xx(statuses.get(400)), true);
  assert.equal(module.is4xx(statuses.get(500)), false);
  assert.equal(module.isFatal4xx(statuses.get(400)), true);
  for (const retryable of [408, 409, 429]) assert.equal(module.isFatal4xx(statuses.get(retryable)), false);

  const client = new Anthropic({
    apiKey: 'helper-contract', // awaken-allow: secret
    baseURL: 'https://managed.invalid',
  });
  const poller = new module.WorkPoller({
    client,
    environmentId: 'env_helper',
    environmentKey: 'environment-helper',
    drain: true,
  });
  assert.equal(poller.signal.aborted, false, `${label}: poller starts live`);
  poller.abort();
  assert.equal(poller.signal.aborted, true, `${label}: poller abort is observable`);

  const runner = new module.SessionToolRunner('session_helper', {
    client,
    tools: [],
    maxIdleMs: 0,
  });
  assert.equal(runner.sessionId, 'session_helper');
  assert.deepEqual(runner.tools, []);
  runner.abort();
  assert.equal(runner.signal.aborted, true, `${label}: runner abort is observable`);

  const worker = new module.EnvironmentWorker({ client, workdir: process.cwd() });
  assert.equal(worker.client, client);
}

function assertAccumulator(module, label) {
  let state = module.accumulateManagedAgentsEvent(undefined, {
    type: 'event_start',
    event: { id: 'message_helper', type: 'agent.message' },
  });
  state = module.accumulateManagedAgentsEvent(state, {
    type: 'event_delta',
    event_id: 'message_helper',
    delta: { index: 0, content: { type: 'text', text: 'hello' } },
  });
  state = module.accumulateManagedAgentsEvent(state, {
    type: 'event_delta',
    event_id: 'message_helper',
    delta: { index: 0, content: { type: 'text', text: ' world' } },
  });
  assert.equal(state.content[0].text, 'hello world', `${label}: ordered delta accumulation`);
  assert.equal(
    module.accumulateManagedAgentsEvent(state, { type: 'session.status_idle' }),
    state,
    `${label}: unrelated events preserve accumulator identity`,
  );
  assert.throws(
    () => module.accumulateManagedAgentsEvent(undefined, {
      type: 'event_delta',
      event_id: 'message_helper',
      delta: { index: 0, content: { type: 'text', text: 'late' } },
    }),
    /before its event_start/u,
  );
  assert.throws(
    () => module.accumulateManagedAgentsEvent(state, {
      type: 'event_delta',
      event_id: 'message_helper',
      delta: { index: 2, content: { type: 'text', text: 'gap' } },
    }),
    /beyond the end of content/u,
  );
}

async function assertAgentToolset(module, Anthropic, label) {
  const parent = mkdtempSync(resolve(tmpdir(), 'managed-helper-contract-'));
  const workdir = join(parent, 'work');
  const outside = join(parent, 'outside.txt');
  const ctx = { workdir, maxFileBytes: 16 * 1024 };
  try {
    const { mkdir, writeFile } = await import('node:fs/promises');
    await mkdir(workdir);
    await writeFile(outside, 'outside');
    const tools = module.betaAgentToolset20260401(ctx);
    assert.deepEqual(tools.map(({ name }) => name).sort(), ['bash', 'edit', 'glob', 'grep', 'read', 'write']);
    assert.equal(new Set(tools.map(({ name }) => name)).size, tools.length, `${label}: unique tool names`);
    for (const tool of tools) {
      assert.equal(tool.type, 'custom', `${label}: ${tool.name} kind`);
      assert.equal(tool.input_schema.type, 'object', `${label}: ${tool.name} schema`);
      assert.equal(typeof tool.run, 'function', `${label}: ${tool.name} runner`);
    }

    const write = module.betaWriteTool(ctx);
    const read = module.betaReadTool(ctx);
    const edit = module.betaEditTool(ctx);
    const glob = module.betaGlobTool(ctx);
    const grep = module.betaGrepTool(ctx);
    await write.run({ file_path: 'note.txt', content: 'alpha\nbeta\n' });
    assert.equal(await read.run({ file_path: 'note.txt' }), 'alpha\nbeta\n');
    assert.equal(await read.run({ file_path: 'note.txt', view_range: [2, 2] }), 'beta');
    await edit.run({ file_path: 'note.txt', old_string: 'beta', new_string: 'gamma' });
    assert.match(await glob.run({ pattern: '**/*.txt' }), /note\.txt/u);
    assert.match(await grep.run({ pattern: 'gamma', path: '.' }), /gamma/u);
    assert.equal(await module.resolvePath(ctx, 'note.txt'), join(workdir, 'note.txt'));
    await assert.rejects(
      () => module.resolvePath(ctx, '../outside.txt'),
      /(?:outside (?:the allowed roots|the session's working directory)|escapes workdir)/u,
    );

    const shell = new module.BashSession(workdir, { PATH: process.env.PATH ?? '' });
    assert.deepEqual(await shell.exec('printf first'), { output: 'first', exitCode: 0 });
    await shell.exec('export MANAGED_HELPER_STATE=preserved');
    assert.deepEqual(
      await shell.exec('printf %s "$MANAGED_HELPER_STATE"'),
      { output: 'preserved', exitCode: 0 },
      `${label}: BashSession preserves shell state`,
    );
    shell.close();
    assert.equal(shell.closed, true);
    await assert.rejects(() => shell.exec('true'), /terminated/u);

    const bash = module.betaBashTool(ctx);
    assert.equal(await bash.run({ command: 'printf helper' }), 'helper');
    await assert.rejects(() => bash.run({ command: '' }), /command is required/u);
    bash.close();

    const cleanupSkills = await module.setupSkills({ workdir });
    assert.equal(typeof cleanupSkills, 'function', `${label}: no-op skill cleanup`);
    await cleanupSkills();
    await assert.rejects(
      () => module.extractSkillArchive(new Response(null), join(workdir, 'skill')),
      /no body/u,
    );
    if (module.resolveSkillVersion) {
      assert.equal(await module.resolveSkillVersion({}, 'skill_helper', '123'), '123');
      const client = {
        beta: { skills: { versions: { list: async function* list() {
          yield { version: '9' };
          yield { version: 'latest' };
          yield { version: '11' };
        } } } },
      };
      assert.equal(await module.resolveSkillVersion(client, 'skill_helper', 'latest'), '11');
      const empty = { beta: { skills: { versions: { list: async function* list() {} } } } };
      await assert.rejects(
        () => module.resolveSkillVersion(empty, 'skill_helper', 'latest'),
        /has no concrete version/u,
      );
    }
    if (module.BashTimeoutError) {
      const timeout = new module.BashTimeoutError(17);
      assert.equal(timeout.name, 'BashTimeoutError');
      assert.equal(timeout.timeoutMs, 17);
    }
    if (module.SessionMemoryStores) {
      assert.equal(module.DEFAULT_MEMORY_SYNC_INTERVAL_MS, 15_000);
      assert.equal(module.MIN_MEMORY_SYNC_INTERVAL_MS, 5_000);
      assert.equal(module.MEMORY_FLUSH_TIMEOUT_MS, 30_000);
      assert.equal(module.MARKER_PATH, '.anthropic-memory-store');
      const cause = new Error('memory cause');
      const memoryError = new module.SessionMemoryError('memory failed', cause);
      assert.equal(memoryError.name, 'SessionMemoryError');
      assert.equal(memoryError.cause, cause);
      const client = new Anthropic({
        apiKey: 'helper-contract', // awaken-allow: secret
        baseURL: 'https://managed.invalid',
      });
      assert.throws(
        () => new module.SessionMemoryStores(client, { workdir, syncIntervalMs: 4_999 }),
        /at least 5000ms/u,
      );
      const stores = new module.SessionMemoryStores(client, { workdir });
      await stores.download({ resources: [] });
      assert.deepEqual(stores.roots, []);
      assert.deepEqual(stores.readOnlyRoots, []);
      await stores.syncIfDue();
      await stores.finish();
      await assert.rejects(() => stores.finish(), /already called/u);
      await stores.dispose();
    }
  } finally {
    rmSync(parent, { recursive: true, force: true });
  }
}

for (const packageAlias of PACKAGE_ALIASES) {
  test(`${packageAlias}: every public Managed helper export has executable semantics`, async () => {
    const sdk = resolveSdkPackage(packageAlias);
    const evidence = managedExportFingerprint(packageAlias, SCOPE, { allowMissing: true });
    const modules = await importedEntrypoints(sdk.root, evidence);
    const label = `${packageAlias}@${sdk.version}`;
    assertExactOwnedSurface(evidence, modules, label);

    const { default: Anthropic } = await import(moduleURL(sdk.root, 'index.mjs'));
    await assertZodHelpers(modules.get('helpers/beta/zod.mjs'), sdk.root, label);
    assertEnvironmentHelpers(modules.get('lib/environments/index.mjs'), Anthropic, label);
    const accumulator = modules.get('lib/sessions/accumulate.mjs');
    if (accumulator) assertAccumulator(accumulator, label);
    await assertAgentToolset(modules.get('tools/agent-toolset/node.mjs'), Anthropic, label);
  });
}

test('the behavior ownership catalog is neither stale nor permissive', () => {
  const observed = new Set();
  for (const packageAlias of PACKAGE_ALIASES) {
    for (const { entrypoint, name } of managedExportFingerprint(packageAlias, SCOPE, {
      allowMissing: true,
    }).exports) observed.add(`${entrypoint}#${name}`);
  }
  const owned = new Set(Object.entries(OWNED_EXPORTS)
    .flatMap(([entrypoint, names]) => names.map((name) => `${entrypoint}#${name}`)));
  assert.deepEqual([...owned].sort(), [...observed].sort());
});
