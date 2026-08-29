// REAL-CLI + REAL-LLM ACP × MCP e2e (no stubs): a managed session runs on the
// ACTUAL catalog-selected ACP adapter pointed at the real KIMI endpoint,
// with a dynamically-injected, vault-bound MCP server (α secretless relay). Proves two
// things the user asked to verify with a real LLM:
//
//   1. DYNAMIC MCP INJECTION reaches a real CLI + real model: the session's `mcp_servers`
//      are staged and projected into the adapter's `session/new` as a loopback relay URL.
//      The adapter carries no vault secret; the host relay authenticates upstream and KIMI
//      drives the tool. The upstream MCP fixture records the requests it actually served.
//   2. CONFIG-HOME ISOLATION: the adapter is pointed at the bound Session
//      Environment's isolated `.acp-config`; the host's real CLI homes are NEVER
//      touched. We run the whole server under a throwaway $HOME so even a
//      misbehaving CLI cannot reach it.
//
// Gated: skips unless a KIMI Anthropic-dialect key is discoverable in ~/.bashrc. It needs
// network + an installed real ACP runtime + the real key, so it is NOT part of the default CI sweep.
//
// Run: (from e2e/)  node acp_real_mcp_kimi_e2e.mjs

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import crypto from 'node:crypto';
import Anthropic from '@anthropic-ai/sdk';
import {
  onlyChildDirectory,
  pass,
  waitForSessionEventReceipt,
  withServer,
} from './harness.mjs';
import {
  applyAcpRuntimeProfile,
  parseAcpRuntimes,
  resolveAcpRuntimeProfiles,
} from './acp_runtime_profiles.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';
import { loadKimiConfig } from './kimi_config.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const CALC_TOKEN = 'calc-bearer-token-e2e'; // awaken-allow: secret

async function main() {
  const kimi = loadKimiConfig();
  if (!kimi) {
    console.log('SKIP: no KIMI ANTHROPIC_API_KEY/BASE_URL found in ~/.bashrc');
    return;
  }

  // A throwaway HOME + storage dir: the CLI's isolated config homes live under storage;
  // the fake HOME guarantees real operator CLI homes are unreachable for the whole run. Pin
  // CARGO_HOME/RUSTUP_HOME to the real home first so the harness's `cargo build` (which
  // resolves them from HOME) still finds the toolchain + dep cache under the fake HOME.
  const realHome = os.homedir();
  const selectedRuntimes = parseAcpRuntimes(process.env.ACP_RUNTIME ?? 'claude');
  assert.equal(selectedRuntimes.length, 1, 'ACP_RUNTIME must select exactly one runtime');
  const [runtime] = selectedRuntimes;
  const noMcp = process.env.ACP_NO_MCP === '1';
  const multiTurn = process.env.ACP_MULTITURN === '1';
  // Runtime/profile decision table: C1=one canonical catalog runtime; C2=Kimi
  // compatibility supports Claude/OpenCode/Hermes; C3=an isolated OpenAI
  // credential supports Codex; C4=Gemini/unknown has neither compatible source.
  // Effects: E1 C1+(C2|C3) resolves one profile; E2 C4 fails before server or
  // paid model work. The shared profile resolver is the sole compatibility
  // owner; this scenario must not mirror runtime/provider branches.
  const [runtimeProfile] = resolveAcpRuntimeProfiles({
    runtimes: [runtime],
    kimi,
    env: {
      OPENAI_BASE_URL: kimi.openaiBase,
      OPENAI_API_KEY: kimi.key,
      OPENAI_MODEL: kimi.openaiModel,
    },
  });
  const sandboxHome = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-home-'));
  const sandboxDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-sandboxes-'));
  const storageDir = fs.mkdtempSync(path.join(os.tmpdir(), 'awaken-acp-store-'));
  // The ACP adapter reads the shared resolver's model/credential projection
  // from the operator env (the ACP model-delivery path).
  Object.assign(process.env, {
    CARGO_HOME: process.env.CARGO_HOME ?? path.join(realHome, '.cargo'),
    RUSTUP_HOME: process.env.RUSTUP_HOME ?? path.join(realHome, '.rustup'),
    HOME: sandboxHome,
    SESSION_DEPLOYMENT_SANDBOX_DIR: sandboxDir,
    SESSION_DEPLOYMENT_STORAGE_DIR: storageDir,
  });
  applyAcpRuntimeProfile(runtimeProfile, process.env);

  // The expected value exists only inside the MCP fixture. Unlike arithmetic,
  // the model cannot manufacture it from the prompt and must really call the
  // dynamically injected tool for the assertion to pass.
  const attestation = `awaken-${crypto.randomBytes(12).toString('hex')}`;
  const conversationMarker = `turn-memory-${crypto.randomBytes(12).toString('hex')}`;
  const fixture = await startCalcFixture(CALC_TOKEN, { opaqueResult: attestation });
  try {
    await withServer('acp-real-mcp', 38198, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl, timeout: 600_000 });
      const acpAgent = await client.beta.agents.create({
        name: `${runtime} live ACP`,
        model: runtimeProfile.model,
        mcp_servers: noMcp ? [] : [{ name: 'calc', type: 'url', url: fixture.url }],
        // Permission decision table: C1 this paid unattended canary enables
        // calc; C2 the authored MCP toolset explicitly owns AlwaysAllow.
        // E1=C1+C2 executes the fixture-only call and can reach Agent Message;
        // E2=C1+!C2 correctly stops at requires_action, which is covered by
        // the dedicated permission-resume suites and cannot satisfy this gate.
        tools: noMcp ? [] : [{
          type: 'mcp_toolset',
          mcp_server_name: 'calc',
          configs: [],
          default_config: {
            enabled: true,
            permission_policy: { type: 'always_allow' },
          },
        }],
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
        environment_id: 'env_local',
        vault_ids: noMcp ? [] : [vault.id],
        betas: BETAS,
      });

      // First-Run decision rule: F1 exact receipt unprocessed => observe; F2
      // processed receipt without a later Agent Message + idle => observe; F3
      // both later effects => assert the scenario-specific model/tool evidence.
      // Older Session history can never satisfy this Run.
      const firstReceipt = await client.beta.sessions.events.send(session.id, {
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
      const firstReceiptId = firstReceipt.data?.[0]?.id;
      assert.equal(typeof firstReceiptId, 'string', 'first real ACP Run exact User receipt');
      const { events } = await waitForSessionEventReceipt(
        client,
        session.id,
        firstReceiptId,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message')
          && delta.some((event) => event.type === 'session.status_idle'),
        `real ${runtime} ACP Run to commit its Agent reply`,
        { timeoutMs: 600_000, pollMs: 200 },
      );
      const texts = events
        .filter((e) => e.type === 'agent.message')
        .map((e) => (e.content ?? []).map((c) => c.text ?? '').join(''));
      console.log('agent messages:', JSON.stringify(texts));
      console.log('fixture methods:', JSON.stringify(fixture.calls.map((c) => c.method)));

      // (1) Dynamic MCP injection reached the REAL adapter's MCP client through
      // the single α relay route. Flow decision table: C1 Host staging performs
      // server/discover; C2 the launched adapter receives the loopback URL;
      // C3 its MCP client completes initialize -> initialized -> list -> call.
      // E1=C1+C2+C3 proves both stages without requiring a duplicate upstream
      // client; E2 any missing/out-of-order method fails. Every upstream request
      // carries the vault token, while the sandboxed CLI receives no credential.
      const methods = fixture.calls.map((call) => call.method);
      const requiredFlow = [
        'server/discover',
        'initialize',
        'notifications/initialized',
        'tools/list',
        'tools/call',
      ];
      if (noMcp) {
        const expected = multiTurn ? 'ACK' : 'OK';
        assert.ok(
          texts.some((t) => t.trim() === expected),
          `expected a plain ACP ${expected} reply, got ${JSON.stringify(texts)}`,
        );
        if (multiTurn) {
          // Second-Run decision rule: M1 exact receipt processed without a
          // later recalled token + idle => observe; M2 both later effects =>
          // prove session/load continuity. First-Run messages are ineligible.
          const secondReceipt = await client.beta.sessions.events.send(session.id, {
            events: [{
              type: 'user.message',
              content: [{
                type: 'text',
                text: 'What exact token did I ask you to remember? Reply with only that token.',
              }],
            }],
            betas: BETAS,
          });
          const secondReceiptId = secondReceipt.data?.[0]?.id;
          assert.equal(typeof secondReceiptId, 'string', 'second real ACP Run exact User receipt');
          const { delta: secondDelta } = await waitForSessionEventReceipt(
            client,
            session.id,
            secondReceiptId,
            BETAS,
            ({ delta }) => delta.some(
              (event) => event.type === 'agent.message'
                && JSON.stringify(event.content).includes(conversationMarker),
            ) && delta.some((event) => event.type === 'session.status_idle'),
            `real ${runtime} ACP second Run to recall prior context`,
            { timeoutMs: 600_000, pollMs: 200 },
          );
          const after = secondDelta
            .filter((event) => event.type === 'agent.message')
            .map((event) => (event.content ?? []).map((content) => content.text ?? '').join(''));
          assert.ok(after.length > 0, 'the second ACP turn produced a new assistant message');
          assert.ok(
            after.at(-1).includes(conversationMarker),
            `${runtime} session/load recalled the first-turn token: ${JSON.stringify(after.at(-1))}`,
          );
          pass(`ACP ${runtime} recalled prior context on a second managed turn`);
        }
        pass(`ACP ${runtime} completed a namespace turn without MCP`);
        return;
      }
      const positions = requiredFlow.map((method) => methods.indexOf(method));
      assert.ok(
        positions.every((position) => position >= 0)
          && positions.every((position, index) => index === 0 || positions[index - 1] < position),
        `host discovery and the relayed ACP MCP flow must be complete and ordered: ${JSON.stringify(methods)}`,
      );
      assert.ok(
        fixture.calls.every((c) => c.authorization === `Bearer ${CALC_TOKEN}`),
        `the host relay authenticated every upstream MCP request, got ${JSON.stringify(fixture.calls.map((c) => c.authorization))}`,
      );
      const calledAttest = fixture.calls.some((c) => c.method === 'tools/call');
      pass(`dynamic MCP injection reached the real ${runtime} adapter + KIMI (host discover + ordered α-relayed MCP flow, tools/call=${calledAttest})`);

      // (2) Config-home isolation decision table: C1 one bound Session
      // Environment exists; C2 its runtime-owned sentinel exists beneath the
      // bound `.acp-config`; C3 the throwaway process HOME contains only fixture
      // cache. E1=C1+C2+C3 proves the CLI used the Session boundary; zero/multiple
      // provider directories or any host-home write fails. `onlyChildDirectory`
      // remains the one oracle for the provider's private scope-name mapping.
      const sessionRoot = onlyChildDirectory(
        sandboxDir,
        'one ACP Session Environment exists',
      );
      const configHome = path.join(sessionRoot, '.acp-config');
      assert.ok(
        fs.existsSync(path.join(configHome, '.awaken-config-home')),
        `the bound Session Environment owns the ACP config home: ${configHome}`,
      );
      const unexpectedHomeEntries = fs.readdirSync(sandboxHome)
        .filter((entry) => entry !== '.npm');
      assert.deepEqual(
        unexpectedHomeEntries,
        [],
        'the ACP run never wrote outside its isolated config home',
      );
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
    fs.rmSync(sandboxDir, { recursive: true, force: true });
    fs.rmSync(storageDir, { recursive: true, force: true });
  }
}

main();
