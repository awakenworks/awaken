// e2e for OAuth credential materialization (#5) over the REAL wire. The Agents'
// profile is bound to a `CredentialKind::Oauth` source whose helper mints a fresh
// short-lived token on every run (`oauth-minted-key-<helper pid>`). The provider
// connection carries its OWN vault key, which the fake provider accepts only for
// its model directory, never for inference: a reply therefore proves the bound
// OAuth source, not the connection key, authenticated the run.
//
// Cause/effect graph. C1 the profile binds the OAuth source; C2 its helper runs
// and the minted token is presented; C3 the provider currently accepts the
// cached token; C4 the process is new (no minted token cached); C5 the
// provider already refused the cached token in this process. Effects: E1 the
// Run replies and every inference presented a helper-minted token (never the
// connection key); E2 within one process the token is minted once and reused;
// E3 a token the provider no longer accepts fails that Run loudly with no
// reply, no silent retry and no fallback to another credential, and the
// refusal retires the cached token; E4 the next Run mints a new token, which
// authenticates, so the refused token is presented at most once per cache
// entry; E5 a new process mints a new token, which authenticates.
//
// | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
// | O1   | Y  | Y  | Y  | Y  | N  | E1 (first token T1) |
// | O2   | Y  | Y  | Y  | N  | N  | E1 + E2 (T1 again) |
// | O3   | Y  | Y  | N  | N  | N  | E3 (T1 refused once, Run fails) |
// | O4   | Y  | Y  | -  | N  | Y  | E1 + E4 (T2 != T1, T1 not presented again) |
// | O5   | Y  | Y  | Y  | Y  | N  | E1 + E5 (T3 not in {T1, T2}) |
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
    let secondToken;
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

      // O3: the provider revokes T1 while it is still cached. The Run that
      // presents it fails loudly; it is neither retried silently nor rescued
      // by any other credential (the connection key included).
      revoked.add(firstToken);
      const revokedAt = upstream.credentials.length;
      mark = revokedAt;
      const refusedBefore = upstream.unauthorized;
      const third = await turn(client, session.id, 'after expiry');
      const o3 = inferenceCredentials(mark);
      assert.deepEqual(o3.map(({ presented, accepted }) => ({ presented, accepted })),
        [{ presented: firstToken, accepted: false }],
        `O3 the revoked token was presented exactly once and refused: ${JSON.stringify(o3)}`);
      assert.ok(upstream.unauthorized > refusedBefore, 'O3 the provider refused the revoked token');
      assert.ok(!replyText(third).includes('FAKE:after expiry'), `O3 no reply was fabricated: ${replyText(third)}`);
      const idle = third.filter((event) => event.type === 'session.status_idle').at(-1);
      assert.notEqual(idle?.stop_reason?.type, 'end_turn', `O3 the Run did not end its turn: ${JSON.stringify(idle)}`);
      assert.ok(third.some((event) => event.type === 'session.error'),
        `O3 the refusal is reported: ${third.map((event) => event.type)}`);
      pass('O3 a revoked cached token is refused once and the Run fails without a credential fallback');

      // O4: the refusal retired the cached token, so the next Run in the same
      // process mints a fresh one through the helper and succeeds, long
      // before the cache TTL would have expired T1.
      mark = upstream.credentials.length;
      const fourth = await turn(client, session.id, 'after refresh');
      assert.ok(replyText(fourth).includes('FAKE:after refresh'), `O4 the run replied: ${replyText(fourth)}`);
      const o4 = inferenceCredentials(mark);
      assert.ok(o4.length >= 1 && o4.every(({ presented, accepted }) => accepted
        && MINTED.test(presented) && presented !== firstToken),
      `O4 a freshly minted token authenticated: ${JSON.stringify(o4)}`);
      secondToken = o4[0].presented;
      const refusedPresentations = inferenceCredentials(revokedAt)
        .filter(({ presented }) => presented === firstToken);
      assert.equal(refusedPresentations.length, 1,
        `O4 the refused token is presented at most once per cache entry: ${JSON.stringify(refusedPresentations)}`);
      pass('O4 a provider refusal retires the cached token and the next Run mints a fresh one');
    }, configuration);

    // O5: a new process holds no minted token; its helper run mints a new one.
    await withServer('oauth-resolved', Number(process.env.E2E_PORT), async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      const mark = upstream.credentials.length;
      const events = await turn(client, session.id, 'fresh process');
      assert.ok(replyText(events).includes('FAKE:fresh process'), `O5 the run replied: ${replyText(events)}`);
      const o5 = inferenceCredentials(mark);
      assert.ok(o5.length >= 1 && o5.every(({ presented, accepted }) => accepted
        && MINTED.test(presented) && presented !== firstToken && presented !== secondToken),
      `O5 a newly minted token authenticated: ${JSON.stringify(o5)}`);
      pass('O5 a new process mints a new token through the helper');
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
