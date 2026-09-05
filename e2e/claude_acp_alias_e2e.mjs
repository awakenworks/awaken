// Actual pinned ACP adapter -> SDK -> MCP. Inference alone is deterministic.
// Decision table: R1 immediate allow -> one invocation. R2 cancel permission,
// terminate process, load exact Session, then allow -> one invocation. R3 the
// same restart followed by denial -> zero invocations. Every row retains alias
// metadata, exact arguments and disabled builtins. R2/R3 inspect actual provider
// input and streaming updates, not only accepted ACP request envelopes.
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';
import { startClaudeInference } from './fixtures/claude_inference_fixture.mjs';

const fixture = fileURLToPath(new URL('./fixtures/claude_alias_mcp_fixture.mjs', import.meta.url));
const executable = fileURLToPath(new URL('./node_modules/@agentclientprotocol/claude-agent-acp/dist/index.js', import.meta.url));
const arguments_ = { command: 'AWAKEN_ALIAS_SENTINEL' };
function startClient(root, provider, decide) {
  const child = spawn(process.execPath, [executable], {
    cwd: root, detached: true, stdio: ['pipe', 'pipe', 'pipe'], env: {
      ...process.env, HOME: root, CLAUDE_CONFIG_DIR: root, ANTHROPIC_API_KEY: 'fixture-key', // awaken-allow: secret -- loopback-only fixture key
      ANTHROPIC_AUTH_TOKEN: '', CLAUDE_CODE_OAUTH_TOKEN: '', ANTHROPIC_BASE_URL: provider.base,
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
  return { rpc, stop, updates, permissions };
}

for (const mode of ['immediate', 'resume-allow', 'resume-deny']) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'awaken-real-acp-alias-'));
  let resumed = false; let emittedAfterResume = false;
  const provider = await startClaudeInference({ invokeTool: (_body, turn) => {
    if (!resumed) return turn === 1;
    if (emittedAfterResume) return false;
    emittedAfterResume = true; return true;
  } });
  const record = path.join(root, 'calls.jsonl'); const clients = [];
  const settings = { cwd: root, mcpServers: [{ name: 'awaken_session', command: process.execPath, args: [fixture, record], env: [] }],
    _meta: { claudeCode: { options: { toolAliases: { Bash: 'mcp__awaken_session__bash' }, tools: [], disallowedTools: ['Bash'], settingSources: [] } } } };
  try {
    let cancelSeen;
    const cancelled = new Promise(resolve => { cancelSeen = resolve; });
    const first = startClient(root, provider, () => { cancelSeen(); return mode === 'immediate' ? 'allow' : 'cancel'; });
    clients.push(first);
    await first.rpc('initialize', { protocolVersion: 1, clientCapabilities: {} });
    const session = await first.rpc('session/new', settings);
    const initial = first.rpc('session/prompt', { sessionId: session.sessionId, prompt: [{ type: 'text', text: 'Invoke Bash once.' }] });
    let active = first;
    if (mode === 'immediate') {
      assert.equal((await initial).stopReason, 'end_turn');
    } else {
      const interrupted = initial.catch(() => undefined);
      await Promise.race([cancelled, initial]);
      assert.equal(first.permissions.length, 1, `${mode}: cancellation at exact permission`);
      await first.stop(); await interrupted;
      resumed = true;
      const beforeResume = provider.requests.length;
      active = startClient(root, provider, () => mode === 'resume-allow' ? 'allow' : 'deny'); clients.push(active);
      await active.rpc('initialize', { protocolVersion: 1, clientCapabilities: {} });
      await active.rpc('session/load', { ...settings, sessionId: session.sessionId });
      const marker = `Resume exact pending request: ${JSON.stringify(arguments_)}`;
      const result = await active.rpc('session/prompt', { sessionId: session.sessionId, prompt: [{ type: 'text', text: marker }] });
      assert.equal(result.stopReason, 'end_turn');
      assert.ok(provider.requests.slice(beforeResume).some(request => JSON.stringify(request.messages).includes('Resume exact pending request:')), `${mode}: actual model receives continuation`);
    }
    const calls = await fs.readFile(record, 'utf8').catch(error => { if (error.code === 'ENOENT') return ''; throw error; });
    assert.equal(calls, mode === 'resume-deny' ? '' : JSON.stringify(arguments_) + '\n', `${mode}: exact side-effect count`);
    assert.ok(active.permissions.length > 0, `${mode}: real permission request`);
    assert.deepEqual(active.permissions.at(-1).rawInput, arguments_, `${mode}: complete approval arguments`);
    assert.ok(active.updates.some(update => update.sessionUpdate === 'tool_call_update' && JSON.stringify(update.rawInput) === JSON.stringify(arguments_)), `${mode}: SDK refines streamed input`);
    console.log(`PASS actual Claude ACP/SDK ${mode}: exact permission, streamed input, invocation count`);
  } finally {
    for (const client of clients) await client.stop();
    await provider.close(); await fs.rm(root, { recursive: true, force: true });
  }
}
