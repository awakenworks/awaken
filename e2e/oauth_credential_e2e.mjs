// e2e for OAuth credential materialization (#5) over the REAL wire. The Agents'
// profile is bound to a `CredentialKind::Oauth` source whose helper mints a fresh
// short-lived token on every run (`oauth-minted-key-<helper pid>`). The provider
// connection carries its OWN vault key, which the fake provider accepts only for
// its model directory, never for inference: a reply therefore proves the bound
// OAuth source, not the connection key, authenticated the run.
//
// Cause/effect graph. C1 the profile binds the OAuth source; C2 its helper runs
// and the minted token is presented; C3 the provider currently accepts that
// token; C4 the process is new (no minted token cached). Effects: E1 the Run
// replies and every inference presented a helper-minted token (never the
// connection key); E2 within one process the token is minted once and reused;
// E3 a token the provider no longer accepts fails the Run loudly with no
// reply and no fallback to another credential; E4 a new process mints a new
// token, which authenticates.
//
// | Rule | C1 | C2 | C3 | C4 | Effect |
// | O1   | Y  | Y  | Y  | Y  | E1 (first token T1) |
// | O2   | Y  | Y  | Y  | N  | E1 + E2 (T1 again) |
// | O3   | Y  | Y  | N  | N  | E3 (T1 refused, nothing else presented) |
// | O4   | Y  | Y  | Y  | Y  | E1 + E4 (T2 != T1) |
//
// Run: (from e2e/)  node oauth_credential_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import {
  modelUpstreamEnv,
  withServer,
  pass,
  waitForSessionEventReceipt,
} from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
// The scenario's OAuth helper prints this prefix plus its own process id.
const MINTED = /^oauth-minted-key-\d+$/u;
// The provider connection's own key: a valid directory credential that the
// provider refuses for inference, so it can never be the reason a Run replies.
const CONNECTION_KEY = 'oauth-e2e-connection-key'; // awaken-allow(secret):
// The OAuth-kind credential source the `oauth-resolved` scenario holds.
const OAUTH_FIXTURE_CREDENTIAL = 'cred:scenario:oauth-fixture';
const isInference = ({ method, pathname }) => method === 'POST' && pathname.endsWith('/messages');

async function turn(client, sessionId, text) {
  const receipt = await client.beta.sessions.events.send(sessionId, {
    events: [{ type: 'user.message', content: [{ type: 'text', text }] }],
    betas: BETAS,
  });
  const receiptId = receipt.data[0]?.id;
  assert.equal(typeof receiptId, 'string', `exact User Event receipt for ${text}`);
  const { delta } = await waitForSessionEventReceipt(
    client,
    sessionId,
    receiptId,
    BETAS,
    ({ delta: events }) => events.some((event) => event.type === 'session.status_idle'
      || event.type === 'session.error'),
    `the OAuth-backed Run for "${text}" to settle`,
  );
  return delta;
}

const replyText = (events) => events.filter((event) => event.type === 'agent.message')
  .flatMap((event) => event.content ?? []).map((block) => block.text ?? '').join('');

async function main() {
  const revoked = new Set();
  const upstream = await startFakeAnthropic((presented, request) => (isInference(request)
    ? MINTED.test(presented) && !revoked.has(presented)
    : presented === CONNECTION_KEY));
  // Credentials presented for inference since `from`, in arrival order.
  const inferenceCredentials = (from) => upstream.credentials.slice(from).filter(isInference);
  const configuration = modelUpstreamEnv(upstream, {
    // The operator connects the provider with its own key, then binds the
    // Agents to the scenario's OAuth-kind credential: the admin API authors
    // only server-owned helpers, so the fixture helper is pre-held.
    apiKey: CONNECTION_KEY,
    bindCredentialSourceId: OAUTH_FIXTURE_CREDENTIAL,
  });
  try {
    let firstToken;
    await withServer('oauth-resolved', Number(process.env.E2E_PORT), async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });

      // O1: the bound OAuth source authenticates; the connection key never does.
      let mark = upstream.credentials.length;
      const first = await turn(client, session.id, 'mint me');
      assert.ok(replyText(first).includes('FAKE:mint me'), `O1 the run replied: ${replyText(first)}`);
      const o1 = inferenceCredentials(mark);
      assert.ok(o1.length >= 1, 'O1 the provider served inference');
      assert.ok(o1.every(({ presented, accepted }) => accepted && MINTED.test(presented)),
        `O1 every inference presented an accepted helper-minted token: ${JSON.stringify(o1)}`);
      firstToken = o1[0].presented;
      pass(`O1 the OAuth helper minted the token that authenticated inference (${o1.length} request(s))`);

      // O2: one process reuses its minted token while it is fresh.
      mark = upstream.credentials.length;
      const second = await turn(client, session.id, 'again');
      assert.ok(replyText(second).includes('FAKE:again'), `O2 the run replied: ${replyText(second)}`);
      const o2 = inferenceCredentials(mark);
      assert.ok(o2.length >= 1 && o2.every(({ presented, accepted }) => accepted && presented === firstToken),
        `O2 the cached minted token authenticated again: ${JSON.stringify(o2)}`);
      pass('O2 the minted token is reused within its freshness window');

      // O3: the provider expires T1. The Run must fail on it, loudly, and must
      // not be rescued by any other credential (the connection key included).
      revoked.add(firstToken);
      mark = upstream.credentials.length;
      const refusedBefore = upstream.unauthorized;
      const third = await turn(client, session.id, 'after expiry');
      const o3 = inferenceCredentials(mark);
      assert.ok(o3.length >= 1, 'O3 the Run reached the provider');
      assert.ok(o3.every(({ presented, accepted }) => !accepted && presented === firstToken),
        `O3 only the expired token was presented and it was refused: ${JSON.stringify(o3)}`);
      assert.ok(upstream.unauthorized > refusedBefore, 'O3 the provider refused the expired token');
      assert.ok(!replyText(third).includes('FAKE:after expiry'), `O3 no reply was fabricated: ${replyText(third)}`);
      const idle = third.filter((event) => event.type === 'session.status_idle').at(-1);
      assert.notEqual(idle?.stop_reason?.type, 'end_turn', `O3 the Run did not end its turn: ${JSON.stringify(idle)}`);
      assert.ok(third.some((event) => event.type === 'session.error'),
        `O3 the refusal is reported: ${third.map((event) => event.type)}`);
      pass('O3 an expired minted token is refused and the Run fails without a credential fallback');
    }, configuration);

    // O4: a new process holds no minted token; its helper run mints a new one.
    await withServer('oauth-resolved', Number(process.env.E2E_PORT), async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      const mark = upstream.credentials.length;
      const events = await turn(client, session.id, 'fresh process');
      assert.ok(replyText(events).includes('FAKE:fresh process'), `O4 the run replied: ${replyText(events)}`);
      const o4 = inferenceCredentials(mark);
      assert.ok(o4.length >= 1 && o4.every(({ presented, accepted }) => accepted
        && MINTED.test(presented) && presented !== firstToken),
      `O4 a newly minted token authenticated: ${JSON.stringify(o4)}`);
      pass('O4 a new process mints a new token through the helper');
    }, configuration);

    assert.ok(upstream.credentials.filter(isInference).every(({ presented }) => presented !== CONNECTION_KEY),
      'the connection key was never presented for inference');
    console.log('E2E PASS: OAuth credential materialization over the real wire (#5).');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    upstream.close();
  }
}

main();
