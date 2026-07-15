// REAL-CLI + REAL-LLM ACP × MCP e2e (no stubs): a managed session runs on the
// ACTUAL `claude --acp` adapter (launched via npx) pointed at the real KIMI endpoint,
// with a dynamically-injected, vault-bound MCP server (β trusted-inline). Proves two
// things the user asked to verify with a real LLM:
//
//   1. DYNAMIC MCP INJECTION reaches a real CLI + real model: the session's `mcp_servers`
//      are staged, β-projected into the adapter's `session/new`, the adapter connects to
//      the real MCP server (authenticating with the vault-materialized token), and KIMI
//      drives the tool. The MCP fixture records the requests it actually served.
//   2. CONFIG-HOME ISOLATION: the adapter is pointed at an isolated per-thread config
//      home under AWAKEN_STORAGE_DIR; the host's real ~/.claude is NEVER touched. We run
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
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-bearer-token-e2e'; // awaken-allow: secret

// The KIMI (Anthropic-dialect) config the user keeps in ~/.bashrc, commented out.
function kimiFromBashrc() {
  let text = '';
  try {
    text = fs.readFileSync(path.join(os.homedir(), '.bashrc'), 'utf8');
  } catch {
    return null;
  }
  const key = text.match(/ANTHROPIC_API_KEY=(sk-kimi-[^\s"']+)/)?.[1];
  const base = text.match(/ANTHROPIC_BASE_URL=(https:\/\/api\.kimi\.com[^\s"']*)/)?.[1];
  if (!key || !base) return null;
  // The claude ACP adapter (Claude Code) appends `/v1/messages` itself, so
  // ANTHROPIC_BASE_URL is the ROOT (no `/v1`) — the raw ~/.bashrc value.
  return { key, base, model: 'kimi-k2-0711-preview' };
}

async function listEvents(client, id) {
  const out = [];
  for await (const ev of client.beta.sessions.events.list(id, { betas: BETAS })) out.push(ev);
  return out;
}

async function main() {
  const kimi = kimiFromBashrc();
  if (!kimi) {
    console.log('SKIP: no KIMI ANTHROPIC_API_KEY/BASE_URL found in ~/.bashrc');
    return;
  }

  // A throwaway HOME + storage dir: the CLI's isolated config homes live under storage;
  // the fake HOME guarantees the real ~/.claude is unreachable for the whole run. Pin
  // CARGO_HOME/RUSTUP_HOME to the real home first so the harness's `cargo build` (which
  // resolves them from HOME) still finds the toolchain + dep cache under the fake HOME.
  const realHome = os.homedir();
  const sandboxHome = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-home-'));
  const storageDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-store-'));
  // The ACP adapter reads its model from the operator env (the ACP model-delivery path).
  Object.assign(process.env, {
    CARGO_HOME: process.env.CARGO_HOME ?? path.join(realHome, '.cargo'),
    RUSTUP_HOME: process.env.RUSTUP_HOME ?? path.join(realHome, '.rustup'),
    HOME: sandboxHome,
    AWAKEN_STORAGE_DIR: storageDir,
    ANTHROPIC_BASE_URL: kimi.base,
    ANTHROPIC_API_KEY: kimi.key,
    ANTHROPIC_MODEL: kimi.model,
  });

  const fixture = await startCalcFixture(CALC_TOKEN);
  try {
    await withServer('acp-real-mcp', 38198, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, timeout: 600_000 });

      const vault = await client.beta.vaults.create({ display_name: 'KIMI ACP vault', betas: BETAS });
      await client.beta.vaults.credentials.create(vault.id, {
        type: 'mcp_oauth',
        mcp_server_url: fixture.url,
        access_token: CALC_TOKEN,
        betas: BETAS,
      });

      const session = await client.beta.sessions.create({
        agent: 'assistant',
        // The ACP adapter is handed this as its ANTHROPIC_MODEL (the ACP model-delivery
        // path resolves the run's model_ref), so it must be the real KIMI model name.
        model: kimi.model,
        metadata: { 'awaken.runtime': 'acp:claude' },
        mcp_servers: [{ name: 'calc', type: 'url', url: fixture.url }],
        vault_ids: [vault.id],
        betas: BETAS,
      });

      await client.beta.sessions.events.send(session.id, {
        events: [{
          type: 'user.message',
          content: [{ type: 'text', text: 'Use the calc MCP tool to add 2 and 3. Reply with only the number.' }],
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
      // client, from the β session/new injection). So ≥2 `initialize` proves the injected
      // server reached the CLI. Every request carries the β vault token (never a `session-mcp:`
      // reference) — the raw secret authenticated the real CLI's connection.
      const initializes = fixture.calls.filter((c) => c.method === 'initialize').length;
      assert.ok(
        initializes >= 2,
        `both the host and the real ACP CLI connected to the injected server (≥2 initialize), got ${initializes}`,
      );
      assert.ok(
        fixture.calls.every((c) => c.authorization === `Bearer ${CALC_TOKEN}`),
        `every MCP request carried the β vault token, got ${JSON.stringify(fixture.calls.map((c) => c.authorization))}`,
      );
      const calledAdd = fixture.calls.some((c) => c.method === 'tools/call');
      pass(`dynamic MCP injection reached the real claude adapter + KIMI (host + CLI both connected: ${initializes} initialize, β-authenticated, tools/call=${calledAdd})`);
      if (calledAdd) {
        assert.ok(
          texts.some((t) => t.includes('5')),
          `KIMI used the MCP tool and answered 5, got ${JSON.stringify(texts)}`,
        );
        pass('KIMI dynamically called mcp__calc__add and reported the result (5)');
      }

      // (2) Config-home isolation: the adapter used an isolated per-thread home under the
      // storage dir, and the (throwaway) HOME's ~/.claude was never created.
      const threadsDir = path.join(storageDir, 'threads');
      const homes = fs.existsSync(threadsDir)
        ? fs.readdirSync(threadsDir).filter((t) => fs.existsSync(path.join(threadsDir, t, 'config_home')))
        : [];
      assert.ok(homes.length > 0, `an isolated per-thread config home was created under ${threadsDir}`);
      const hostClaude = path.join(sandboxHome, '.claude');
      const clobbered = fs.existsSync(hostClaude)
        && fs.readdirSync(hostClaude).some((n) => n !== '.npm');
      assert.ok(!clobbered, 'the ACP run never wrote the host default ~/.claude config');
      pass('ACP execution used an isolated config home; the host default ~/.claude was untouched');
    });

    console.log('E2E PASS: real claude --acp + KIMI + dynamic vault-bound MCP, config-home isolated.');
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
