// Actual pinned ACP adapter -> SDK -> MCP. Inference alone is deterministic.
// Decision table: R1 immediate allow -> one invocation. R2 cancel permission,
// terminate process, load exact Session, then allow -> one invocation. R3 the
// same restart followed by denial -> zero invocations. R4/R5 repeat three
// approval boundaries, with prior completed work surviving each restart; the
// last request is allowed/denied -> exactly three/two effects, never a replay.
// R6/R7 use the same three boundaries but first send session/cancel and await
// the prompt acknowledgement before reaping; both final allow/deny must retain
// the same exact effect history. R2-R5 retain abrupt-stop coverage.
// Each model emission has a fresh wire id. Every row retains alias
// metadata, exact arguments and disabled builtins. R2/R3 inspect actual provider
// input and streaming updates, not only accepted ACP request envelopes.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const fixture = fileURLToPath(new URL('./fixtures/claude_alias_mcp_fixture.mjs', import.meta.url));
const executable = fileURLToPath(new URL('./node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js', import.meta.url));
const commandArgumentsFor = ordinal => ({ command: `AWAKEN_ALIAS_SENTINEL_${ordinal}` });
function startClient(root, provider, decide) {
  const child = spawn(process.execPath, [executable], {
    cwd: root, detached: true, stdio: ['pipe', 'pipe', 'pipe'], env: {
      ...process.env, HOME: root, CLAUDE_CONFIG_DIR: root, ANTHROPIC_API_KEY: 'fixture-key', // awaken-allow(secret): -- loopback-only fixture key
      ANTHROPIC_AUTH_TOKEN: '', CLAUDE_CODE_OAUTH_TOKEN: '', ANTHROPIC_BASE_URL: provider.url,
      ANTHROPIC_MODEL: 'claude-sonnet-4-6', CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1',
    },
  });
  let stderr = ''; let stopped = false; let nextId = 1; const pending = new Map(); const updates = []; const permissions = [];
  const send = message => child.stdin.write(JSON.stringify(message) + '\n');
  const exited = new Promise(resolve => child.once('exit', resolve));
  child.stderr.on('data', chunk => { stderr += chunk; });
  readline.createInterface({ input: child.stdout }).on('line', line => {
    const frame = JSON.parse(line);
    if (frame.method === 'session/request_permission') {
      permissions.push(frame.params.toolCall);
      const decision = decide(frame.params.toolCall);
      const option = frame.params.options.find(option => option.kind === (decision === 'deny' ? 'reject_once' : 'allow_once'));
      send({ jsonrpc: '2.0', id: frame.id, result: { outcome: decision === 'cancel'
        ? { outcome: 'cancelled' } : { outcome: 'selected', optionId: option.optionId } } });
    } else if (frame.method === 'session/update') {
      updates.push(frame.params.update);
    } else if (pending.has(frame.id)) {
      const { resolve, reject, timer } = pending.get(frame.id); clearTimeout(timer); pending.delete(frame.id);
      if (frame.error) reject(new Error(JSON.stringify(frame.error))); else resolve(frame.result);
    }
  });
  function rpc(method, params) {
    const id = nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { pending.delete(id); reject(new Error(`${method} timed out: ${stderr.slice(-2000)}`)); }, 60000);
      pending.set(id, { resolve, reject, timer }); send({ jsonrpc: '2.0', id, method, params });
    });
  }
  async function stop() {
    if (stopped) return;
    stopped = true;
    for (const { timer, reject } of pending.values()) { clearTimeout(timer); reject(new Error('fixture process stopped')); }
    pending.clear();
    try { process.kill(-child.pid, 'SIGTERM'); } catch {}
    const timer = setTimeout(() => { try { process.kill(-child.pid, 'SIGKILL'); } catch {} }, 3000);
    try { await exited; } finally { clearTimeout(timer); }
  }
  return { rpc, stop, updates, permissions,
    cancel: sessionId => send({jsonrpc:'2.0',method:'session/cancel',params:{sessionId}}) };
}

for (const [mode, boundaries, finalDecision, settle = false, primed = false, longWrite] of [
  ['immediate', 0, 'allow'], ['resume-allow', 1, 'allow'], ['resume-deny', 1, 'deny'],
  ['repeated-allow', 3, 'allow'], ['repeated-deny', 3, 'deny'],
  ['settled-allow', 3, 'allow', true], ['settled-deny', 3, 'deny', true],
  ['primed-settled', 1, 'allow', true, true],
  // R9-R11: long Write containing quotes, backslashes, Unicode and newlines
  // crosses cancel -> stop -> load -> permission. Exact re-emission + allow
  // dispatches unchanged bytes once (R9); changed bytes + an independent deny
  // dispatches nothing (R10); no re-emission produces no new ask/effect despite
  // a complete approval prompt reaching inference (R11). R11 is a negative
  // protocol capability check, NOT a passing Cowork delivery/recovery claim.
  ['long-write-exact', 1, 'allow', true, false, 'exact'],
  ['long-write-changed-deny', 1, 'deny', true, false, 'changed'],
  ['long-write-no-reissue', 1, 'allow', true, false, 'omit'],
]) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'awaken-real-acp-alias-'));
  const alias = longWrite ? 'Write' : 'Bash';
  const target = `mcp__awaken_session__${alias.toLowerCase()}`;
  const argumentsFor = longWrite ? ordinal => ({
    file_path: path.join(root, `approval-${ordinal}.py`),
    content: '# 精确文件字节：不执行本脚本\n'
      + 'value = "引号 \\" 与反斜线 \\\\"\n'.repeat(900)
      + `# AWAKEN_ALIAS_SENTINEL_${ordinal}\n`,
  }) : commandArgumentsFor;
  if (longWrite) assert.ok(argumentsFor(1).content.length > 16073);
  const emittedArguments = ordinal => longWrite === 'changed' && phase > 0
    ? {...argumentsFor(ordinal), content: argumentsFor(ordinal).content + '\n'}
    : argumentsFor(ordinal);
  let phase = 0; let emissions = 0; let primerEmitted = false; let primerHistoryRetained = true; let expectedPrompt = ''; let promptObserved = false;
  // S1 replacement survives every existing cancellation/load boundary and is
  // present exactly once in the actual provider system field, never User text.
  const instructionMarker = `AWAKEN_FROZEN_SYSTEM_${mode}`;
  const provider = await startFakeAnthropic('fixture-key', { behavior: (body, turn) => {
    const system = typeof body.system === 'string' ? body.system : (body.system ?? []).map(block => block.text ?? '').join('\n');
    assert.equal(system.split(instructionMarker).length - 1, 1, `${mode}/${phase}: exact system delivery`);
    assert.ok(!JSON.stringify(body.messages).includes(instructionMarker), 'no first-prompt duplication');
    promptObserved ||= body.messages.some(message => (typeof message.content === 'string' ? [message.content] : message.content.filter(block => block.type === 'text').map(block => block.text)).some(text => text.includes(expectedPrompt)));
    // R8 a successful operation precedes the very first pending approval.
    // On resume, verify its actual successful tool result reached the provider;
    // a side-effect file alone cannot prove that SDK conversation was retained.
    if (primed && phase > 0 && body.tools?.length) {
      const blocks = body.messages.flatMap(message => Array.isArray(message.content) ? message.content : []);
      const primer = blocks.find(block => block.type === 'tool_use' && block.input?.command === argumentsFor(0).command);
      primerHistoryRetained &&= Boolean(primer && blocks.some(block =>
        block.type === 'tool_result' && block.tool_use_id === primer.id && !block.is_error));
    }
    if (primed && phase === 0 && !primerEmitted) {
      primerEmitted = true;
      return {tool:{id:`toolu_primer_${turn}`,name:'Bash',input:argumentsFor(0)}};
    }
    // One pending request before the first restart. Each resumed prompt first
    // reissues its exact request; only after its result can it ask for the next.
    const ordinal = Math.max(phase, 1) + emissions++;
    const invoke = !(longWrite === 'omit' && phase > 0)
      && (emissions === 1 || (phase > 0 && phase < boundaries && emissions === 2));
    return invoke ? { tool:{ id:`toolu_alias_${turn}`, name:alias, input:emittedArguments(ordinal) } }
      : { text:'finished' };
  } });
  const record = path.join(root, 'calls.jsonl'); const clients = [];
  const settings = { cwd: root, mcpServers: [{ name: 'awaken_session', command: process.execPath, args: [fixture, record], env: [] }],
    _meta: { systemPrompt: instructionMarker, claudeCode: { options: { toolAliases: { [alias]: target }, tools: [], disallowedTools: ['Bash', 'Write'], settingSources: [] } } } };
  const recorded = async () => (await fs.readFile(record, 'utf8').catch(error => {
    if (error.code === 'ENOENT') return ''; throw error;
  })).trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  try {
    let sessionId;
    for (phase = 0; phase <= boundaries; phase++) {
      emissions = 0;
      let cancelSeen;
      const cancelled = new Promise(resolve => { cancelSeen = resolve; });
      const expectsCancellation = phase < boundaries;
      const active = startClient(root, provider, call => {
        if (primed && phase === 0 && call.rawInput?.command === argumentsFor(0).command) return 'allow';
        const ordinal = phase === 0 ? 1 : phase;
        const approved = phase > 0 && JSON.stringify(call.rawInput) === JSON.stringify(argumentsFor(ordinal));
        if (expectsCancellation && !approved) { cancelSeen(); return 'cancel'; }
        return phase === boundaries ? finalDecision : 'allow';
      });
      clients.push(active);
      await active.rpc('initialize', { protocolVersion: 1, clientCapabilities: {} });
      if (phase === 0) sessionId = (await active.rpc('session/new', settings)).sessionId;
      else await active.rpc('session/load', { ...settings, sessionId });
      const loadedUpdateCount = active.updates.length;
      const marker = phase === 0 ? `Invoke ${alias} once.`
        : `Resume exact pending request: ${JSON.stringify(argumentsFor(phase))}`;
      expectedPrompt = marker; promptObserved = false;
      const prompt = active.rpc('session/prompt', { sessionId, prompt: [{ type: 'text', text: marker }] });
      if (expectsCancellation) {
        const interrupted = prompt.catch(() => undefined);
        await Promise.race([cancelled, prompt]);
        assert.deepEqual(active.permissions.at(-1)?.rawInput, argumentsFor(phase + 1), `${mode}/${phase}: exact cancelled request`);
        if (settle) {
          active.cancel(sessionId);
          await interrupted;
        }
        await active.stop(); await interrupted;
      } else {
        assert.equal((await prompt).stopReason, 'end_turn');
        await active.stop();
      }
      assert.ok(promptObserved, `${mode}/${phase}: actual model receives this prompt`);
      assert.ok(primerHistoryRetained, `${mode}: completed primer remains in actual provider history`);
      const noReissue = longWrite === 'omit' && phase > 0;
      const completed = boundaries === 0 ? 1 : phase - (phase === boundaries && (finalDecision === 'deny' || noReissue) ? 1 : 0);
      assert.deepEqual(await recorded(), [...(primed ? [argumentsFor(0)] : []), ...Array.from({ length:completed }, (_, index) => argumentsFor(index + 1))], `${mode}/${phase}: exact cumulative effects, no replay`);
      if (noReissue) {
        assert.equal(active.permissions.length, 0, 'approval text does not resume a cancelled tool by itself');
        // session/load legitimately replays historical tool cards. Only a new
        // call after this prompt could claim resumed execution; MCP records
        // independently check that no old or new invocation happened.
        assert.ok(!active.updates.slice(loadedUpdateCount).some(update => update.sessionUpdate === 'tool_call'), 'no hidden SDK replay without model tool-use');
        continue;
      }
      assert.ok(active.permissions.length > 0, `${mode}/${phase}: real permission request`);
      // I1 alias streaming titles are presentation; the actual SDK permission
      // request must identify the resolved MCP target independently of them.
      for (const permission of active.permissions) {
        assert.equal(permission.title, target, `${mode}: canonical permission identity`);
        if (longWrite) assert.deepEqual(permission.rawInput, emittedArguments(1), `${mode}: full permission arguments`);
      }

      assert.ok(active.updates.some(update => update.sessionUpdate === 'tool_call_update' && JSON.stringify(update.rawInput) === JSON.stringify(emittedArguments(Math.max(phase, 1)))), `${mode}/${phase}: SDK refines complete streamed input`);
    }
    console.log(`PASS actual Claude ACP/SDK ${mode}: ${boundaries} restarts, exact permission/input/effect history`);
  } finally {
    for (const client of clients) await client.stop();
    await provider.close(); await fs.rm(root, { recursive:true, force:true });
  }
}

const systemLengths = new Map();
for (const mode of ['replace', 'append', 'empty_replace']) {
  // Causes: S2 replace/append; S3 fresh process new/load; S4 builtins removed/
  // retained. Rules replace=>one marker+no Bash; append=>one marker+Bash.
  // Both S3 branches reapply identical metadata without a user-message copy.
  // Empty replacement uses the pinned adapter's truthy newline; its provider
  // system stays no larger than custom replacement and does not regain builtins.
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'awaken-acp-system-'));
  const marker = `AWAKEN_SYSTEM_ROUTE_${mode}`;
  let observed = 0;
  const provider = await startFakeAnthropic('fixture-key', { behavior: body => {
    const system = typeof body.system === 'string' ? body.system : (body.system ?? []).map(block => block.text ?? '').join('\n');
    assert.equal(system.split(marker).length - 1, mode === 'empty_replace' ? 0 : 1, `${mode}: system marker count`);
    assert.ok(!JSON.stringify(body.messages).includes(marker), `${mode}: no User duplicate`);
    assert.equal((body.tools ?? []).some(tool => tool.name === 'Bash'), mode === 'append', `${mode}: builtin policy preserved`);
    systemLengths.set(mode, system.length);
    if (mode === 'empty_replace') assert.ok(system.length <= systemLengths.get('replace'), 'empty transport drops the same preset as nonempty replacement');
    observed++;
    return { text: 'system-route-observed' };
  } });
  let active;
  try {
    const settings = { cwd: root, mcpServers: [], _meta: {
      systemPrompt: mode === 'append' ? { append: marker } : mode === 'empty_replace' ? '\n' : marker,
      claudeCode: { options: { settingSources: [], ...(mode !== 'append' ? { tools: [] } : {}) } },
    } };
    let sessionId;
    for (const method of ['session/new', 'session/load']) {
      active = startClient(root, provider, () => 'deny');
      await active.rpc('initialize', { protocolVersion: 1, clientCapabilities: {} });
      const session = await active.rpc(method, { ...settings, ...(sessionId ? { sessionId } : {}) });
      sessionId ??= session.sessionId;
      assert.equal((await active.rpc('session/prompt', { sessionId, prompt: [{ type: 'text', text: 'Say system-route-observed.' }] })).stopReason, 'end_turn');
      await active.stop();
    }
    assert.ok(observed >= 2, 'new and fresh-process load reached inference');
    console.log(`PASS actual Claude ACP system ${mode}: new/load, marker=${mode === 'empty_replace' ? 0 : 1}, builtin policy preserved`);
  } finally { await active?.stop(); await provider.close(); await fs.rm(root, { recursive: true, force: true }); }
}
