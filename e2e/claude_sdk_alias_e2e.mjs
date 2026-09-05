// Actual pinned Claude SDK and its shipped executable; only inference is a
// loopback deterministic provider. No account or external model call is used.
// Cause/effect graph: model-emitted alias + map + registered canonical MCP tool
// -> one exact invocation; missing map -> no invocation. Builtins are disabled
// in both cases, so accidental native Bash execution cannot satisfy the proof.
import assert from 'node:assert/strict';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';
import fs from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { query, tool, createSdkMcpServer } from '@anthropic-ai/claude-agent-sdk';
import { z } from 'zod';

async function run(enabled, allow) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'awaken-sdk-alias-'));
  let calls = 0; const inputs = [];
  let canonicalAdvertised = false; let builtinAdvertised = false;
  const provider = await startFakeAnthropic('fixture-key', { behavior:(body, turn) => {
    canonicalAdvertised ||= body.tools.some(tool => tool.name === 'mcp__awaken_session__bash');
    builtinAdvertised ||= body.tools.some(tool => tool.name === 'Bash');
    return turn === 1 ? { tool:{ id:`toolu_alias_${turn}`, name:'Bash', input:{ command:'AWAKEN_ALIAS_SENTINEL' } } } : { text:'finished' };
  } });
  const mcp = createSdkMcpServer({ name:'awaken_session', tools:[tool('bash','Record a test command without executing it',{ command:z.string() },async (input) => { calls++; inputs.push(input); return { content:[{ type:'text',text:'AWAKEN_ALIAS_OK' }] }; })] });
  const abortController = new AbortController(); const timer = setTimeout(() => abortController.abort(), 60000);
  try {
    const stream = query({ prompt:'Invoke Bash once with command AWAKEN_ALIAS_SENTINEL.', options:{
      cwd:root, model:'claude-sonnet-4-6', settingSources:[], tools:[], disallowedTools:['Bash'], maxTurns:3,
      abortController, mcpServers:{ awaken_session:mcp },
      ...(enabled?{ toolAliases:{ Bash:'mcp__awaken_session__bash' } }:{}),
      env:{ ...process.env, ANTHROPIC_API_KEY:'fixture-key', ANTHROPIC_AUTH_TOKEN:'', CLAUDE_CODE_OAUTH_TOKEN:'', ANTHROPIC_BASE_URL:provider.url, CLAUDE_CONFIG_DIR:root, HOME:root, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC:'1' }, // awaken-allow: secret -- loopback-only fixture key, no provider account
      canUseTool:async (_name,input) => allow ? ({ behavior:'allow',updatedInput:input }) : ({ behavior:'deny',message:'fixture denied' }),
    } });
    const events = []; for await (const event of stream) events.push(event);
    assert.ok(provider.requests.length >= 2, 'actual SDK completed the injected tool-use round trip');
    assert.equal(calls, enabled && allow?1:0, `alias=${enabled} dispatch count`);
    if (enabled && allow) assert.deepEqual(inputs,[{ command:'AWAKEN_ALIAS_SENTINEL' }], 'alias does not transform arguments');
    assert.ok(canonicalAdvertised, 'canonical target registered');
    assert.ok(!builtinAdvertised, 'builtin removed from advertised tools');
    assert.ok(events.some(event => event.type === 'result'), 'SDK produced a terminal result');
    console.log(`PASS actual Claude SDK 0.3.232 alias=${enabled}, allow=${allow}, calls=${calls}, inference=loopback`);
  } finally { clearTimeout(timer); await provider.close(); await fs.rm(root,{ recursive:true,force:true }); }
}
await run(true,true); await run(false,true); await run(true,false);
