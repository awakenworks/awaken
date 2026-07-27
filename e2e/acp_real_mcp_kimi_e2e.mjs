// REAL-CLI + REAL-LLM ACP × MCP e2e (no stubs): a managed session runs on the
// ACTUAL `claude --acp` adapter (launched via npx) pointed at the real KIMI endpoint,
// with a dynamically-injected, vault-bound MCP server (α secretless relay). Proves two
// things the user asked to verify with a real LLM:
//
//   1. DYNAMIC MCP INJECTION reaches a real CLI + real model: the session's `mcp_servers`
//      are staged and projected into the adapter's `session/new` as a loopback relay URL.
//      The adapter carries no vault secret; the host relay authenticates upstream and KIMI
//      drives the tool. The upstream MCP fixture records the requests it actually served.
//   2. CONFIG-HOME ISOLATION: the adapter is pointed at an isolated per-thread config
//      home under SESSION_DEPLOYMENT_STORAGE_DIR; the host's real ~/.claude is NEVER touched. We run
//      the whole server under a throwaway $HOME so even a misbehaving CLI cannot reach it.
//
// Gated: skips unless a KIMI Anthropic-dialect key is discoverable in ~/.bashrc. It needs
// network + npx + the real key, so it is NOT part of the default CI sweep.
//
// Run: (from e2e/)  node acp_real_mcp_kimi_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import { loadKimiConfig } from './kimi_config.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-bearer-token-e2e'; // awaken-allow: secret

async function listEvents(client, id) {
  const out = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) out.push(ev);
  return out;
}

async function main() {
  const kimi = loadKimiConfig();
  if (!kimi) {
    console.log('SKIP: no KIMI ANTHROPIC_API_KEY/BASE_URL found in ~/.bashrc');
    return;
  }

  // A throwaway HOME + storage dir: the CLI's isolated config homes live under storage;
  // the fake HOME guarantees the real ~/.claude is unreachable for the whole run. Pin
  // CARGO_HOME/RUSTUP_HOME to the real home first so the harness's `cargo build` (which
  // resolves them from HOME) still finds the toolchain + dep cache under the fake HOME.
  const realHome = os.homedir();
  const runtime = process.env.ACP_RUNTIME ?? 'claude';
  const noMcp = process.env.ACP_NO_MCP === '1';
  const multiTurn = process.env.ACP_MULTITURN === '1';
  const supported = new Set(['claude', 'kimi', 'opencode', 'hermes', 'codex']);
  assert.ok(supported.has(runtime), `ACP_RUNTIME must be one of ${[...supported].join(', ')}`);
  const sandboxHome = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-home-'));
  const storageDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-store-'));
  // The ACP adapter reads its model from the operator env (the ACP model-delivery path).
  Object.assign(process.env, {
    CARGO_HOME: process.env.CARGO_HOME ?? path.join(realHome, '.cargo'),
    RUSTUP_HOME: process.env.RUSTUP_HOME ?? path.join(realHome, '.rustup'),
    HOME: sandboxHome,
    SESSION_DEPLOYMENT_STORAGE_DIR: storageDir,
    ANTHROPIC_BASE_URL: kimi.anthropicBase,
    ANTHROPIC_API_KEY: kimi.anthropicKey ?? kimi.key,
    ANTHROPIC_MODEL: kimi.anthropicModel,
    AWAKEN_ACP_CLI: runtime,
    AWAKEN_MODEL: runtime === 'claude' ? kimi.anthropicModel : kimi.openaiModel,
  });
  if (runtime === 'kimi') {
    Object.assign(process.env, {
      KIMI_MODEL_BASE_URL: kimi.openaiBase,
      KIMI_MODEL_API_KEY: kimi.key,
      KIMI_MODEL_NAME: kimi.openaiModel,
    });
  } else if (runtime === 'opencode' || runtime === 'codex') {
    Object.assign(process.env, {
      OPENAI_BASE_URL: kimi.openaiBase,
      OPENAI_API_KEY: kimi.key,
      OPENAI_MODEL: kimi.openaiModel,
    });
    if (runtime === 'opencode') {
      Object.assign(process.env, {
      OPENCODE_CONFIG_CONTENT: JSON.stringify({
        model: `awaken-kimi/${kimi.openaiModel}`,
        small_model: `awaken-kimi/${kimi.openaiModel}`,
        enabled_providers: ['awaken-kimi'],
        provider: {
          'awaken-kimi': {
            npm: '@ai-sdk/openai-compatible',
            name: 'Awaken Kimi Code',
            options: { baseURL: kimi.openaiBase, apiKey: '{env:OPENAI_API_KEY}' }, // awaken-allow: secret
            models: { [kimi.openaiModel]: { name: kimi.openaiModel } },
          },
        },
      }),
      });
    }
  } else if (runtime === 'hermes') {
    Object.assign(process.env, {
      KIMI_BASE_URL: kimi.openaiBase,
      KIMI_API_KEY: kimi.key,
      HERMES_MODEL: kimi.openaiModel,
    });
  }

  // The expected value exists only inside the MCP fixture. Unlike arithmetic,
  // the model cannot manufacture it from the prompt and must really call the
  // dynamically injected tool for the assertion to pass.
  const attestation = `awaken-${crypto.randomBytes(12).toString('hex')}`;
  const conversationMarker = `turn-memory-${crypto.randomBytes(12).toString('hex')}`;
  const fixture = await startCalcFixture(CALC_TOKEN, { opaqueResult: attestation });
  try {
    await withServer('acp-real-mcp', 38198, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, timeout: 600_000 });
      const selectedModel = runtime === 'claude' ? kimi.anthropicModel : kimi.openaiModel;
      const acpAgent = await client.beta.agents.create({
        name: `${runtime} live ACP`,
        model: selectedModel,
        betas: BETAS,
      });

      const vault = await client.beta.vaults.create({ display_name: 'KIMI ACP vault', betas: BETAS });
      await client.beta.vaults.credentials.create(vault.id, {
        type: 'mcp_oauth',
        mcp_server_url: fixture.url,
        access_token: CALC_TOKEN,
        betas: BETAS,
      });

      const session = await client.beta.sessions.create({
        agent: acpAgent.id,
        // The ACP adapter is handed this as its ANTHROPIC_MODEL (the ACP model-delivery
        // path resolves the run's model_ref), so it must be the real KIMI model name.
        model: selectedModel,
        mcp_servers: noMcp ? [] : [{ name: 'calc', type: 'url', url: fixture.url }],
        vault_ids: noMcp ? [] : [vault.id],
        betas: BETAS,
      });

      await client.beta.sessions.events.send(session.id, {
        events: [{
          type: 'user.message',
          content: [{
            type: 'text',
            text: noMcp
              ? (multiTurn
                  ? `Remember this exact token for the next turn: ${conversationMarker}. Reply with only ACK.`
                  : 'Reply with only OK. Do not call tools.')
              : 'Use the available tool named mcp__calc__attest, then reply with only its opaque audit token. The token is not present in this prompt and must not be guessed.',
          }],
        }],
        betas: BETAS,
      });

      const events = await listEvents(client, session.id);
      const texts = events
        .filter((e) => e.type === 'agent.message')
        .map((e) => (e.content ?? []).map((c) => c.text ?? '').join(''));
      console.log('agent messages:', JSON.stringify(texts));
      console.log('fixture methods:', JSON.stringify(fixture.calls.map((c) => c.method)));

      // (1) Dynamic MCP injection reached the REAL adapter's own MCP client — not just the
      // host's in-process `connect_staged`. Two independent MCP clients handshake with the
      // fixture: the host (tool discovery/pre-auth) AND the launched claude adapter (its own
      // client, through the α relay injected into session/new). So ≥2 `initialize` proves
      // the injected server reached the CLI. Every upstream request carries the vault token,
      // but only the host relay materializes it; the sandboxed CLI receives a loopback URL.
      const initializes = fixture.calls.filter((c) => c.method === 'initialize').length;
      if (noMcp) {
        const expected = multiTurn ? 'ACK' : 'OK';
        assert.ok(
          texts.some((t) => t.trim() === expected),
          `expected a plain ACP ${expected} reply, got ${JSON.stringify(texts)}`,
        );
        if (multiTurn) {
          const before = texts.length;
          await client.beta.sessions.events.send(session.id, {
            events: [{
              type: 'user.message',
              content: [{
                type: 'text',
                text: 'What exact token did I ask you to remember? Reply with only that token.',
              }],
            }],
            betas: BETAS,
          });
          const after = (await listEvents(client, session.id))
            .filter((event) => event.type === 'agent.message')
            .map((event) => (event.content ?? []).map((content) => content.text ?? '').join(''));
          assert.ok(after.length > before, 'the second ACP turn produced a new assistant message');
          assert.ok(
            after.at(-1).includes(conversationMarker),
            `${runtime} session/load recalled the first-turn token: ${JSON.stringify(after.at(-1))}`,
          );
          pass(`ACP ${runtime} recalled prior context on a second managed turn`);
        }
        pass(`ACP ${runtime} completed a namespace turn without MCP`);
        return;
      }
      assert.ok(
        initializes >= 2,
        `both the host and the real ACP CLI connected to the injected server (≥2 initialize), got ${initializes}`,
      );
      assert.ok(
        fixture.calls.every((c) => c.authorization === `Bearer ${CALC_TOKEN}`),
        `the host relay authenticated every upstream MCP request, got ${JSON.stringify(fixture.calls.map((c) => c.authorization))}`,
      );
      const calledAttest = fixture.calls.some((c) => c.method === 'tools/call');
      pass(`dynamic MCP injection reached the real claude adapter + KIMI (host + α-relayed CLI both connected: ${initializes} initialize, tools/call=${calledAttest})`);

      // (2) Config-home isolation: the adapter used an isolated per-thread home under the
      // storage dir, and the (throwaway) HOME's ~/.claude was never created.
      const threadsDir = path.join(storageDir, 'threads');
      const homes = fs.existsSync(threadsDir)
        ? fs.readdirSync(threadsDir).filter((t) => fs.existsSync(path.join(threadsDir, t, 'config_home')))
        : [];
      assert.ok(homes.length > 0, `an isolated per-thread config home was created under ${threadsDir}`);
      const defaultHomes = ['.claude', '.kimi-code', '.config/opencode', '.hermes'];
      const clobbered = defaultHomes.some((relative) => {
        const target = path.join(sandboxHome, relative);
        return fs.existsSync(target) && fs.readdirSync(target).some((n) => n !== '.npm');
      });
      assert.ok(!clobbered, 'the ACP run never wrote a CLI default config home');
      pass(`ACP ${runtime} execution used an isolated config home`);

      // Keep transport/config assertions independent from provider semantics so a
      // remote quota failure still diagnoses the completed portions of the chain.
      // The overall gate succeeds only when the real model actually uses the tool.
      assert.ok(calledAttest, `KIMI must call mcp__calc__attest, got ${JSON.stringify(fixture.calls.map((c) => c.method))}`);
      assert.ok(
        texts.some((t) => t.includes(attestation)),
        `KIMI must return the fixture-only attestation token, got ${JSON.stringify(texts)}`,
      );
      pass('KIMI dynamically called mcp__calc__attest and reported its fixture-only token');
    });

    console.log(`E2E PASS: real ${runtime} ACP + KIMI + dynamic vault-bound MCP, config-home isolated.`);
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await fixture.close();
    fs.rmSync(sandboxHome, { recursive: true, force: true });
    fs.rmSync(storageDir, { recursive: true, force: true });
  }
}

main();
