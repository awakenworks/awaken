#!/usr/bin/env node

import { spawn } from 'node:child_process';
import readline from 'node:readline';

const MARKER = 'AWAKEN-PLAYWRIGHT-MCP-OK';

function write(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function stdioCoordinates(server) {
  const value = server?.stdio ?? server?.transport ?? server ?? {};
  return {
    command: value.command ?? server?.command,
    args: value.args ?? server?.args ?? [],
    env: Object.fromEntries((value.env ?? server?.env ?? []).map((entry) => [entry.name, entry.value])),
  };
}

async function provePlaywright(server) {
  const { command, args, env } = stdioCoordinates(server);
  if (!command) throw new Error(`playwright MCP command missing: ${JSON.stringify(server)}`);
  const child = spawn(command, args, {
    env: { ...process.env, ...env },
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  let stderr = '';
  child.stderr.setEncoding('utf8');
  child.stderr.on('data', (chunk) => { stderr += chunk; });

  const pending = new Map();
  const output = readline.createInterface({ input: child.stdout });
  output.on('line', (line) => {
    let message;
    try { message = JSON.parse(line); } catch { return; }
    if (message.id != null && pending.has(message.id)) {
      const { resolve, reject } = pending.get(message.id);
      pending.delete(message.id);
      if (message.error) reject(new Error(JSON.stringify(message.error)));
      else resolve(message.result);
    }
  });
  child.once('exit', (code, signal) => {
    for (const { reject } of pending.values()) {
      reject(new Error(`playwright MCP exited code=${code} signal=${signal}: ${stderr}`));
    }
    pending.clear();
  });

  let nextId = 1;
  const request = (method, params = {}) => new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', id, method, params })}\n`);
  });
  const timeout = setTimeout(() => child.kill('SIGKILL'), 45_000);
  try {
    await request('initialize', {
      protocolVersion: '2024-11-05',
      capabilities: {},
      clientInfo: { name: 'awaken-environment-e2e', version: '1.0.0' },
    });
    child.stdin.write(`${JSON.stringify({ jsonrpc: '2.0', method: 'notifications/initialized' })}\n`);
    const tools = await request('tools/list');
    const names = (tools.tools ?? []).map((tool) => tool.name);
    for (const required of ['browser_navigate', 'browser_evaluate', 'browser_snapshot']) {
      if (!names.includes(required)) throw new Error(`missing ${required}: ${JSON.stringify(names)}`);
    }
    await request('tools/call', { name: 'browser_navigate', arguments: { url: 'about:blank' } });
    const evaluated = await request('tools/call', {
      name: 'browser_evaluate',
      arguments: {
        function: `() => { document.title = '${MARKER}'; document.body.innerHTML = '<main>${MARKER}</main>'; return document.title; }`,
      },
    });
    const snapshot = await request('tools/call', { name: 'browser_snapshot', arguments: {} });
    const proof = `${JSON.stringify(evaluated)} ${JSON.stringify(snapshot)}`;
    if (!proof.includes(MARKER)) throw new Error(`browser proof absent: ${proof}`);
  } finally {
    clearTimeout(timeout);
    child.kill('SIGTERM');
    output.close();
  }
}

let sessionRequest;
const input = readline.createInterface({ input: process.stdin });
input.on('line', async (line) => {
  let request;
  try { request = JSON.parse(line); } catch { return; }
  if (request.id === 1) {
    write({ jsonrpc: '2.0', id: 1, result: { protocolVersion: 1, agentCapabilities: {} } });
    return;
  }
  if (request.id === 2) {
    sessionRequest = request;
    write({ jsonrpc: '2.0', id: 2, result: { sessionId: 'playwright-e2e' } });
    return;
  }
  if (request.id !== 3) return;
  try {
    const servers = sessionRequest?.params?.mcpServers ?? [];
    const playwright = servers.find((server) => server.name === 'playwright');
    if (!playwright) throw new Error(`playwright MCP route absent: ${JSON.stringify(servers)}`);
    await provePlaywright(playwright);
    write({
      jsonrpc: '2.0',
      method: 'session/update',
      params: {
        sessionId: 'playwright-e2e',
        update: {
          sessionUpdate: 'agent_message_chunk',
          content: { type: 'text', text: MARKER },
        },
      },
    });
    write({ jsonrpc: '2.0', id: 3, result: { stopReason: 'end_turn' } });
  } catch (error) {
    process.stderr.write(`${error.stack ?? error}\n`);
    write({ jsonrpc: '2.0', id: 3, error: { code: -32000, message: String(error.message ?? error) } });
  }
});
