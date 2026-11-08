// e2e for OAuth credential materialization (#5) over the REAL wire: the agent's
// credential source is `CredentialKind::Oauth`, so resolving it runs the helper
// (`printf oauth-minted-key`) to mint a short-lived Bearer token. The fake upstream
// authenticates EXACTLY that minted token, so a run succeeds only if the OAuth
// materialize path actually ran the helper and used its output as the API key.
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
// The token the server's OAuth helper mints (`printf oauth-minted-key`). The
// upstream accepts only this, so a valid run proves the helper ran.
const MINTED_KEY = 'oauth-minted-key'; // awaken-allow(secret):
// The OAuth-kind credential source the `oauth-resolved` scenario holds.
const OAUTH_FIXTURE_CREDENTIAL = 'cred:scenario:oauth-fixture';

async function main() {
  const upstream = await startFakeAnthropic(MINTED_KEY);
  try {
    // The only credential bound is the OAuth-kind source: the upstream accepts
    // nothing but its minted token, so a reply proves the helper ran.
    await withServer('oauth-resolved', Number(process.env.E2E_PORT), async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      // C1=exact OAuth-backed User receipt; C2=authenticated reply+terminal.
      // E1=C2 after C1 proves minted material reached the provider. K: ambient
      // key is deliberately wrong. Decision O1 C1&&!C2=>retry; O2 C1+C2=>proof.
      const receipt = await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'mint me' }] }],
        betas: BETAS,
      });

      const receiptId = receipt.data[0]?.id;
      assert.equal(typeof receiptId, 'string', 'O1 exact OAuth-backed User Event receipt');
      const { delta: events } = await waitForSessionEventReceipt(
        client,
        session.id,
        receiptId,
        BETAS,
        ({ delta }) => delta.some((event) => event.type === 'agent.message')
          && delta.some((event) => event.type === 'session.status_idle'),
        'O1 OAuth-backed Run to commit its authenticated reply',
      );
      const msg = events.find((e) => e.type === 'agent.message');
      assert.ok(msg, `expected an agent.message in ${events.map((e) => e.type)}`);
      const text = (msg.content ?? []).map((c) => c.text ?? '').join('');

      // The run reached the upstream and got a reply — the minted token worked.
      assert.ok(text.includes('FAKE:mint me'), `the OAuth-credentialed run hit the wire: ${text}`);
      assert.equal(upstream.unauthorized, 0, 'the OAuth-minted token authenticated (no 401s)');
      assert.ok(upstream.requests.length >= 1, 'the upstream served the OAuth-credentialed inference');
      pass('an OAuth-kind credential mints its token via the helper and authenticates the run (#5)');
    }, modelUpstreamEnv(upstream, {
      // The operator connects the provider (an Anthropic connection takes an
      // API key), then binds the Agents to the scenario's OAuth-kind credential:
      // the admin API authors only server-owned helpers, so the fixture helper
      // is pre-held. Only the bound credential can authenticate the Run.
      apiKey: MINTED_KEY,
      bindCredentialSourceId: OAUTH_FIXTURE_CREDENTIAL,
    }));

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
