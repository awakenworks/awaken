// Managed Agents capability-advertisement e2e with the official Anthropic TypeScript
// SDK. Proves the created session's agent object carries the *official* wire shapes:
// the built-in tools fold into one `agent_toolset_20260401` reference, client tools
// are `custom` definitions, a delegate roster is a `coordinator` multiagent object,
// and mcp_servers / resources are empty. The SDK deserializes the session at each
// step, so a shape the SDK can't place would surface here.
//
// Run: (from e2e/)  npm install && node managed_capabilities_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withRealServer, withScenarioServer } from './harness.mjs';

const BETAS = ['managed-agents-2026-04-01'];

// The scenario's host config is what these assertions read (client tools, delegate
// roster); the model runs for real over the wire. Each mode maps to the behavior
// reproducing its model — irrelevant here (no turns) but kept faithful.
const BEHAVIOR = { echo: 'echo', custom: 'custom', delegate: 'delegating' };

// Boot the server in `mode` (its host config) with the model on the real wire, then
// run `fn(client)`. `echo` is a plain-mount scenario (real mode); the others keep
// their `AWAKEN_MODEL_MODE=<mode>` router with the model swapped to the real wire.
async function withServer(mode, port, fn) {
  const wrap = (baseUrl) => fn(new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl }));
  return mode === 'echo'
    ? withRealServer('echo', port, wrap)
    : withScenarioServer(mode, BEHAVIOR[mode], port, wrap);
}

function toolset(session) {
  const t = session.agent.tools.find((x) => x.type === 'agent_toolset_20260401');
  assert.ok(t, `agent_toolset_20260401 present in ${JSON.stringify(session.agent.tools)}`);
  return t;
}

async function main() {
  try {
    // --- echo: built-in toolset fold, empty everything else ---
    await withServer('echo', 38101, async (client) => {
      const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      const ts = toolset(s);
      // The toolset carries a resolved `default_config` (required by the SDK): the
      // baseline every non-overridden tool inherits — enabled + auto-allow.
      assert.deepEqual(ts.default_config, {
        enabled: true,
        permission_policy: { type: 'always_allow' },
      });
      // Capability-member graph: C1=the current SDK models each built-in config
      // as a discriminated union member; C2=the registered local member is
      // perception or a controlled modification; C3=a configured plugin has no
      // routed execution owner. E1=perception inherits enabled+allow from the
      // one toolset default; E2=controlled modification emits enabled+ask; E3=C3
      // emits disabled+allow. K=`type` must identify the same member as `name`.
      // This keeps execution admission and advertised wire policy on the one
      // Session-owned toolset authority.
      //
      // | Rule | member | classification / route | wire effect |
      // | R1 | read/glob/grep | perception | inherit default (no config) |
      // | R2 | bash/write/edit | controlled modification | enabled + always_ask |
      // | R3 | web_fetch/web_search | no routed plugin | disabled + always_allow |
      const cfg = Object.fromEntries(ts.configs.map((c) => [c.name, c]));
      for (const inherited of ['read', 'glob', 'grep']) {
        assert.ok(!(inherited in cfg), `${inherited} inherits the exact Agent-toolset default`);
      }
      for (const controlled of ['bash', 'write', 'edit']) {
        assert.deepEqual(cfg[controlled], {
          name: controlled,
          type: controlled,
          enabled: true,
          permission_policy: { type: 'always_ask' },
        });
      }
      assert.deepEqual(cfg.web_fetch, {
        name: 'web_fetch',
        type: 'web_fetch',
        enabled: false,
        permission_policy: { type: 'always_allow' },
      });
      assert.deepEqual(cfg.web_search, {
        name: 'web_search',
        type: 'web_search',
        enabled: false,
        permission_policy: { type: 'always_allow' },
      });
      assert.deepEqual(s.agent.mcp_servers, []);
      assert.deepEqual(s.agent.skills, []);
      assert.deepEqual(s.resources, []);
      assert.ok(s.agent.multiagent == null, 'no multiagent without a roster');
      console.log('  ok: echo -> agent_toolset fold, empty mcp/skills/resources, no multiagent');
    });

    // --- custom: a client tool appears as a `custom` tool definition ---
    await withServer('custom', 38102, async (client) => {
      const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      toolset(s); // built-in toolset still present
      const custom = s.agent.tools.find((x) => x.type === 'custom');
      assert.ok(custom, `a custom tool is advertised in ${JSON.stringify(s.agent.tools)}`);
      assert.equal(custom.name, 'submit_answer');
      assert.equal(typeof custom.input_schema, 'object');
      console.log('  ok: custom -> {type:custom, name:submit_answer, input_schema}');
    });

    // --- delegate: the roster becomes a coordinator multiagent object ---
    await withServer('delegate', 38103, async (client) => {
      const s = await client.beta.sessions.create({ agent: 'assistant', environment_id: 'env_local', betas: BETAS });
      assert.equal(s.agent.multiagent.type, 'coordinator');
      assert.ok(
        s.agent.multiagent.agents.some((agent) => agent.id === 'researcher'),
        `roster: ${JSON.stringify(s.agent.multiagent.agents)}`,
      );
      console.log('  ok: delegate -> coordinator roster contains the resolved researcher Agent');
    });

    console.log('E2E PASS: Managed Agents capability advertisement matches the official wire via TS SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  }
}

main();
