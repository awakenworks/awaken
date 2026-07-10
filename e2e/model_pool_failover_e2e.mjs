// e2e for model-pool failover (#1) over the REAL wire: the session's primary model
// (`ANTHROPIC_MODEL=fail-model`) is failed by the fake upstream with a retryable
// overloaded error; the run exhausts its retries and fails over to the ordered pool
// fallback (`AWAKEN_MODEL_FALLBACKS=ok-model`), which serves the turn. The `label`
// behavior echoes the model that actually answered, so the reply proves which model
// served — and the upstream request log proves both were tried in order.
//
// Run: (from e2e/)  node model_pool_failover_e2e.mjs

import assert from 'node:assert/strict';
import Anthropic from '@anthropic-ai/sdk';
import { withServer, pass } from './harness.mjs';
import { startFakeAnthropic } from './fixtures/fake_anthropic_fixture.mjs';

const BETAS = ['managed-agents-2026-04-01'];
const FAKE_KEY = 'sk-fake-failover-key'; // awaken-allow: secret

async function main() {
  // The upstream echoes the model (`label`) and fails exactly `fail-model`.
  const upstream = await startFakeAnthropic(FAKE_KEY, { behavior: 'label', failModel: 'fail-model' });
  try {
    process.env.ANTHROPIC_API_KEY = FAKE_KEY;
    process.env.ANTHROPIC_BASE_URL = `${upstream.url}/v1/`;
    process.env.ANTHROPIC_MODEL = 'fail-model';
    process.env.AWAKEN_MODEL_SOURCE = 'http';
    process.env.AWAKEN_MODEL_FALLBACKS = 'ok-model';

    await withServer('pool-failover', 38266, async (baseUrl) => {
      const client = new Anthropic({ apiKey: 'e2e-dummy', baseURL: baseUrl });
      const session = await client.beta.sessions.create({
        agent: 'assistant',
        environment_id: 'env_local',
        betas: BETAS,
      });
      await client.beta.sessions.events.send(session.id, {
        events: [{ type: 'user.message', content: [{ type: 'text', text: 'route me' }] }],
        betas: BETAS,
      });

      const events = [];
      for await (const ev of client.beta.sessions.events.list(session.id, { betas: BETAS })) events.push(ev);
      const msg = events.find((e) => e.type === 'agent.message');
      assert.ok(msg, `expected an agent.message in ${events.map((e) => e.type)}`);
      const text = (msg.content ?? []).map((c) => c.text ?? '').join('');

      // Failover reached the fallback: the reply is served by `ok-model`, not the
      // down primary.
      assert.ok(text.includes('model=ok-model'), `expected failover to ok-model, got: ${text}`);
      assert.ok(!text.includes('model=fail-model'), `the down model must not serve: ${text}`);

      // Both candidates were tried in order — the primary failed, the fallback served.
      const models = upstream.requests.map((r) => r.model);
      assert.ok(models.includes('fail-model'), `the primary was attempted: ${models}`);
      assert.ok(models.includes('ok-model'), `the fallback was attempted: ${models}`);
      assert.ok(
        upstream.requests.some((r) => r.model === 'fail-model' && r.failed),
        'the primary model was recorded as failed at the upstream',
      );
      assert.ok(
        upstream.requests.findIndex((r) => r.model === 'fail-model') <
          upstream.requests.findIndex((r) => r.model === 'ok-model'),
        'the primary was tried before the fallback (ordered failover)',
      );
      pass('a run whose primary model is down fails over to its pool fallback (#1)');
    });

    console.log('E2E PASS: model-pool failover across candidate bindings over the real wire (#1).');
    process.exitCode = 0;
  } catch (err) {
    console.error('E2E FAIL:', err);
    process.exitCode = 1;
  } finally {
    upstream.close();
  }
}

main();
