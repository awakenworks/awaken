import assert from 'node:assert/strict';
import { waitForValue } from '../harness.mjs';

// 0.125: an Environment worker runs a work item on the sessions token carried
// by the item's secret; the environment key is needed only when no token can
// be extracted. Cause/effect graph: C1 the secret carries `sessions_token`;
// C2 an environment key is configured. Effects: E1 the per-item client sends
// exactly that token as its bearer; E2 the item is refused before any request.
// | Rule | C1 | C2 | Effect |
// | K1   | T  | F  | E1 (a host holding only the per-item secret runs) |
// | K2   | F  | F  | E2 (never runs unauthenticated)                 |
export async function exerciseEnvironmentWorkerSecretToken({ Anthropic, EnvironmentWorker }) {
  const workSecretOf = (payload) => Buffer.from(JSON.stringify(payload), 'utf8').toString('base64url');
  const observed = [];
  const client = new Anthropic({
    apiKey: 'environment-key-absent', // awaken-allow: secret
    baseURL: 'https://managed.invalid',
    maxRetries: 0,
    fetch: async (input, init) => {
      const request = new Request(input, init);
      observed.push({ url: request.url, authorization: request.headers.get('authorization') });
      throw new Error('worker-secret-token-probe: transport reached');
    },
  });
  const worker = new EnvironmentWorker({ client, environmentId: 'env_candidate' });
  const item = { workId: 'work_candidate', environmentId: 'env_candidate', sessionId: 'session_candidate' };
  const ctrl = new AbortController();
  const run = worker.handleItem({ ...item, workSecret: workSecretOf({ sessions_token: 'token_from_secret' }), signal: ctrl.signal }) // awaken-allow: secret
    .then(() => 'completed', (error) => String(error));
  await waitForValue(() => observed.length, (count) => count > 0,
    'worker secret request reached the observed transport', { timeoutMs: 10_000 });
  ctrl.abort();
  await run;
  assert.ok(observed.length > 0, 'K1 the item reached transport without an environment key');
  assert.equal(observed[0].authorization, 'Bearer token_from_secret', 'K1 exact per-item bearer');
  assert.ok(observed.every(({ authorization }) => authorization === 'Bearer token_from_secret'), 'K1 no fallback credential');
  const requestsBeforeRejection = observed.length;
  await assert.rejects(
    () => worker.handleItem({ ...item, workSecret: workSecretOf({ note: 'no token' }) }),
    /no sessions token could be extracted/u,
    'K2 a secret without a token and no key is refused',
  );
  assert.equal(observed.length, requestsBeforeRejection, 'K2 refusal happened before any request');
}
