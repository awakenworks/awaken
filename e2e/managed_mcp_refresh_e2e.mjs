// Managed-agents MCP OAuth REFRESH + live-validate e2e driven by the
// **official** Anthropic TypeScript SDK. A vault-held `mcp_oauth` credential
// starts with an EXPIRED access token plus a public-client refresh
// configuration ({client_id, refresh_token, token_endpoint}, no client_secret):
// the first connect hits the 401 challenge, the host refresher exchanges the
// fixed refresh token at the fixture's `/token` endpoint
// (`grant_type=refresh_token&refresh_token=...&client_id=...` form-encoded),
// and the turn still succeeds with `Bearer new-token` on the wire. The fresh
// token is RESEALED under the credential, so later turns AND a second session
// connect with zero additional grants, and `mcpOAuthValidate` now live-probes
// it `valid`. The invalid arm pins the fail-closed side: a wrong token with NO
// refresh validates `invalid` (http_status 401) and fails the turn loudly with
// the api_error envelope naming the challenge. Mirrors the in-process Rust
// tests in crates/agents/awaken-server-local/tests/mcp_sessions.rs.
//
// Run: (from e2e/)  npm install && node managed_mcp_refresh_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startCalcFixture } from './fixtures/mcp_calc_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const EXPIRED_TOKEN = 'expired-token-e2e'; // awaken-allow: secret
const REFRESH_TOKEN = 'rt-fixed-e2e'; // awaken-allow: secret
const NEW_TOKEN = 'new-token'; // awaken-allow: secret
const WRONG_TOKEN = 'wrong-token-e2e'; // awaken-allow: secret
const CLIENT_ID = 'cli-e2e';

async function listEvents(client, sessionId) {
  const events = [];
  for await (const ev of client.beta.sessions.events.list(sessionId, { betas: BETAS })) events.push(ev);
  return events;
}

async function sendMessage(client, sessionId, text) {
  await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
}

const agentMessages = (events) =>
  events.filter((e) => e.type === 'agent.message').map((e) => e.content[0].text);

/// Assert one add-turn: tool_use mcp__calc__add + tool_result <sum> + "result: <sum>".
function assertAddTurn(events, sum) {
  assert.ok(
    events.some((e) => e.type === 'agent.tool_use' && e.name === 'mcp__calc__add'),
    `an mcp__calc__add tool_use: ${JSON.stringify(events.map((e) => e.type))}`,
  );
  assert.ok(
    events.some((e) => e.type === 'agent.tool_result' && e.content[0].text === String(sum)),
    `a tool_result of ${sum}`,
  );
  assert.ok(
    agentMessages(events).some((m) => m.includes(`result: ${sum}`)),
    `a final message reporting result: ${sum} — got ${JSON.stringify(agentMessages(events))}`,
  );
}

async function main() {
  // Fixture A: the "expired initial token" mode — EXPIRED_TOKEN always 401s;
  // only the /token grant (fixed REFRESH_TOKEN) issues the accepted NEW_TOKEN.
  const fixtureA = await startCalcFixture(EXPIRED_TOKEN, {
    expiredInitial: true,
    refreshToken: REFRESH_TOKEN,
    issueToken: NEW_TOKEN,
  });
  // Fixture B: a plain static-bearer instance whose accepted token the invalid
  // arm's credential does NOT hold (and no refresh escape hatch).
  const fixtureB = await startCalcFixture('calc-b-accepted-token'); // awaken-allow: secret
  try {
    await withServer('management', 38192, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });

      // --- vault + refreshable mcp_oauth credential (all secrets write-only) ---
      const vault = await client.beta.vaults.create({ display_name: 'MCP refresh vault', betas: BETAS });
      assert.equal(vault.type, 'vault');
      const cred = await client.beta.vaults.credentials.create(vault.id, {
        type: 'mcp_oauth',
        mcp_server_url: fixtureA.url,
        access_token: EXPIRED_TOKEN,
        refresh: {
          client_id: CLIENT_ID,
          refresh_token: REFRESH_TOKEN,
          token_endpoint: fixtureA.tokenUrl,
          token_endpoint_auth: { type: 'none' }, // public client — no client_secret
          scope: 'tools',
        },
        betas: BETAS,
      });
      assert.equal(cred.type, 'vault_credential');
      assert.equal(cred.auth.type, 'mcp_oauth');
      assert.equal(cred.auth.mcp_server_url, fixtureA.url);
      assert.deepEqual(
        cred.auth.refresh,
        {
          client_id: CLIENT_ID,
          token_endpoint: fixtureA.tokenUrl,
          token_endpoint_auth: { type: 'none' },
          scope: 'tools',
        },
        'the refresh projection is configuration-only',
      );
      const credJson = JSON.stringify(cred);
      assert.ok(!credJson.includes(EXPIRED_TOKEN), 'access token must not be echoed');
      assert.ok(!credJson.includes(REFRESH_TOKEN), 'refresh token must not be echoed');
      pass('credentials.create(mcp_oauth + refresh) -> secret-free, refresh config echoed');

      // --- (a) session 1, turn 1: the expired token is refreshed mid-connect ---
      const session1 = await client.beta.sessions.create({
        agent: 'assistant',
        mcp_servers: [{ name: 'calc', type: 'url', url: fixtureA.url }],
        vault_ids: [vault.id],
        betas: BETAS,
      });
      await sendMessage(client, session1.id, 'add 2 3');
      assertAddTurn(await listEvents(client, session1.id), 5);
      pass('turn 1: add 2 3 -> result 5 despite the expired initial token');

      // Exactly one grant, carrying the documented form-encoded wire.
      assert.equal(fixtureA.grants.length, 1, `one challenge, one grant — got ${fixtureA.grants.length}`);
      const grant = fixtureA.grants[0];
      assert.ok(
        grant.contentType.includes('application/x-www-form-urlencoded'),
        `the grant is form-encoded — got ${grant.contentType}`,
      );
      const form = new URLSearchParams(grant.body);
      assert.equal(form.get('grant_type'), 'refresh_token', grant.body);
      assert.equal(form.get('refresh_token'), REFRESH_TOKEN, grant.body);
      assert.equal(form.get('client_id'), CLIENT_ID, grant.body);
      assert.equal(form.get('scope'), 'tools', grant.body);
      pass('exactly one grant: grant_type=refresh_token&refresh_token=...&client_id=...&scope=tools');

      // The expired bearer was presented exactly once (the challenged
      // handshake); the actual tools/call carried the freshly issued token.
      assert.equal(
        fixtureA.tokenRequests[EXPIRED_TOKEN], 1,
        `the expired bearer is never sent again after the refresh — counts ${JSON.stringify(fixtureA.tokenRequests)}`,
      );
      let toolCalls = fixtureA.calls.filter((c) => c.method === 'tools/call');
      assert.equal(toolCalls.length, 1);
      assert.equal(toolCalls[0].authorization, `Bearer ${NEW_TOKEN}`, 'tools/call carried Bearer new-token');
      pass('expired bearer seen exactly once; tools/call carried `Bearer new-token`');

      // --- (b) turn 2 on the SAME session: zero additional grants ---
      await sendMessage(client, session1.id, 'add 40 2');
      assertAddTurn(await listEvents(client, session1.id), 42);
      assert.equal(fixtureA.grants.length, 1, 'no new grant for the second turn');
      pass('turn 2 (same session): add 40 2 -> result 42 with ZERO additional grants');

      // --- (b) a SECOND session on the same vault/credential: the RESEALED
      // token materializes and connects with zero additional grants ---
      const session2 = await client.beta.sessions.create({
        agent: 'assistant',
        mcp_servers: [{ name: 'calc', type: 'url', url: fixtureA.url }],
        vault_ids: [vault.id],
        betas: BETAS,
      });
      await sendMessage(client, session2.id, 'add 1 2');
      assertAddTurn(await listEvents(client, session2.id), 3);
      assert.equal(fixtureA.grants.length, 1, 'the resealed token connects the second session with no new grant');
      assert.equal(
        fixtureA.tokenRequests[EXPIRED_TOKEN], 1,
        'the second session never presented the expired token',
      );
      toolCalls = fixtureA.calls.filter((c) => c.method === 'tools/call');
      assert.equal(toolCalls.length, 3, `three tools/call in total, got ${toolCalls.length}`);
      for (const c of toolCalls) assert.equal(c.authorization, `Bearer ${NEW_TOKEN}`);
      pass('second session: add 1 2 -> result 3 from the RESEALED token, still one grant total');

      // --- (c) mcpOAuthValidate live-probes the resealed credential valid ---
      const validation = await client.beta.vaults.credentials.mcpOAuthValidate(cred.id, {
        vault_id: vault.id,
        betas: BETAS,
      });
      assert.equal(validation.type, 'vault_credential_validation');
      assert.equal(validation.status, 'valid');
      assert.equal(validation.has_refresh_token, true);
      assert.deepEqual(validation.mcp_probe, { handshake: 'ok' });
      const validationJson = JSON.stringify(validation);
      assert.ok(!validationJson.includes(NEW_TOKEN), 'the probe detail never carries the token');
      assert.ok(!validationJson.includes(REFRESH_TOKEN), 'nor the refresh token');
      pass('mcpOAuthValidate -> valid, has_refresh_token=true, mcp_probe {handshake:"ok"}');

      // --- (d) invalid arm: wrong token, NO refresh, second fixture instance ---
      const vault2 = await client.beta.vaults.create({ display_name: 'MCP invalid vault', betas: BETAS });
      const badCred = await client.beta.vaults.credentials.create(vault2.id, {
        type: 'mcp_oauth',
        mcp_server_url: fixtureB.url,
        access_token: WRONG_TOKEN,
        betas: BETAS,
      });
      const badValidation = await client.beta.vaults.credentials.mcpOAuthValidate(badCred.id, {
        vault_id: vault2.id,
        betas: BETAS,
      });
      assert.equal(badValidation.status, 'invalid');
      assert.equal(badValidation.has_refresh_token, false);
      assert.deepEqual(badValidation.mcp_probe, { http_status: 401 });
      pass('mcpOAuthValidate (wrong token, no refresh) -> invalid, mcp_probe {http_status:401}');

      // The turn against it fails LOUDLY with the api_error envelope naming
      // the server and the unresolved challenge — never silence.
      const session3 = await client.beta.sessions.create({
        agent: 'assistant',
        mcp_servers: [{ name: 'calc', type: 'url', url: fixtureB.url }],
        vault_ids: [vault2.id],
        betas: BETAS,
      });
      await assert.rejects(
        () => sendMessage(client, session3.id, 'add 2 3'),
        (err) => {
          assert.equal(err.status, 500, `an api_error envelope — got ${err.status}: ${err.message}`);
          const envelope = err.error?.error ?? err.error;
          assert.equal(envelope.type, 'api_error', JSON.stringify(err.error));
          assert.ok(envelope.message.includes('mcp server `calc`'), envelope.message);
          assert.ok(envelope.message.includes('auth challenge: HTTP 401'), envelope.message);
          return true;
        },
      );
      assert.equal(fixtureB.grants.length, 0, 'no refresh config -> no grant was ever attempted');
      pass('turn against the wrong-token credential fails with api_error naming `auth challenge: HTTP 401`');
    });

    console.log('E2E PASS: mcp_oauth refresh exchange, reseal persistence, and live validate round-trip through the official @anthropic-ai/sdk.');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    await fixtureA.close();
    await fixtureB.close();
  }
}

main();
