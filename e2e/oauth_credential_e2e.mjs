// e2e for OAuth credential materialization (#5) over the REAL wire: the agent's
// credential source is `CredentialKind::Oauth`, so resolving it runs the helper
// (`printf oauth-minted-key`) to mint a short-lived Bearer token. The fake upstream
// authenticates EXACTLY that minted token, so a run succeeds only if the OAuth
// materialize path actually ran the helper and used its output as the API key.
//
// Run: (from e2e/)  node oauth_credential_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
// The token the server's OAuth helper mints (`printf oauth-minted-key`). The
// upstream accepts only this, so a valid run proves the helper ran.
const MINTED_KEY = 'oauth-minted-key'; // awaken-allow: secret

async function main() {
  const upstream = await startFakeAnthropic(MINTED_KEY);
  try {
    process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
    process.env.ANTHROPIC_MODEL = 'fake-haiku';
    // A deliberately WRONG ambient key: if the run authenticated with this instead
    // of the OAuth-minted token, the upstream would 401. Success proves it did not.
    process.env.ANTHROPIC_API_KEY = 'sk-not-the-minted-key'; // awaken-allow: secret

    await withServer('oauth-resolved', 38270, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'mint me' }] }],
        betas: BETAS,
      });

      const events = [];
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
      const msg = events.find((e) => e.type === 'agent.message');
      assert.ok(msg, `expected an agent.message in ${events.map((e) => e.type)}`);
      const text = (msg.content ?? []).map((c) => c.text ?? '').join('');

      // The run reached the upstream and got a reply — the minted token worked.
      assert.ok(text.includes('FAKE:mint me'), `the OAuth-credentialed run hit the wire: ${text}`);
      assert.equal(upstream.unauthorized, 0, 'the OAuth-minted token authenticated (no 401s)');
      assert.ok(upstream.requests.length >= 1, 'the upstream served the OAuth-credentialed inference');
      pass('an OAuth-kind credential mints its token via the helper and authenticates the run (#5)');
    });

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
