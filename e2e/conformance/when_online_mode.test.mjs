import assert from 'node:assert/strict';
import test from 'node:test';
import {
  officialAnthropicClientOptions,
  whenOnlineMode,
} from './when_online_mode.mjs';

test('local conformance remains an explicit clean skip', () => {
  assert.deepEqual(whenOnlineMode({}), { run: false, error: null });
});

test('release mode cannot silently skip a missing credential', () => {
  assert.equal(
    whenOnlineMode({ AWAKEN_WHEN_ONLINE_REQUIRED: '1' }).error,
    'official Managed online gate requires ANTHROPIC_API_KEY, '
      + 'AWAKEN_WHEN_ONLINE_AGENT, AWAKEN_WHEN_ONLINE_ENV',
  );
});

test('release mode runs with complete fixtures even without the optional opt-in flag', () => {
  assert.deepEqual(
    whenOnlineMode({
      AWAKEN_WHEN_ONLINE_REQUIRED: '1',
      ANTHROPIC_API_KEY: 'secret', // awaken-allow: secret
      AWAKEN_WHEN_ONLINE_AGENT: 'agent_reference',
      AWAKEN_WHEN_ONLINE_ENV: 'environment_reference',
    }),
    { run: true, error: null },
  );
});

test('a credential alone never opts a developer into billable external work', () => {
  assert.deepEqual(whenOnlineMode({ ANTHROPIC_API_KEY: 'secret' }), { run: false, error: null });
});

test('an opted-in partial fixture fails before external work', () => {
  // Decision table: no opt-in + ambient key => skip; either opt-in mode + any
  // missing fixture => error; either opt-in mode + all fixtures => run. This
  // prevents local placeholder identities from becoming reference evidence.
  const mode = whenOnlineMode({
    AWAKEN_WHEN_ONLINE: '1',
    ANTHROPIC_API_KEY: 'secret', // awaken-allow: secret
  });
  assert.equal(mode.run, false);
  assert.match(mode.error, /AWAKEN_WHEN_ONLINE_AGENT, AWAKEN_WHEN_ONLINE_ENV/u);
});

test('developer opt-in runs only with one complete official fixture tuple', () => {
  assert.deepEqual(whenOnlineMode({
    AWAKEN_WHEN_ONLINE: '1',
    ANTHROPIC_API_KEY: 'secret', // awaken-allow: secret
    AWAKEN_WHEN_ONLINE_AGENT: 'agent_reference',
    AWAKEN_WHEN_ONLINE_ENV: 'environment_reference',
  }), { run: true, error: null });
});

test('official evidence cannot be redirected by an ambient SDK base URL', () => {
  // Causal graph: ambient gateway -> official-client option projection -> SDK.
  // The projection must discard the gateway so only api.anthropic.com can
  // produce live-reference evidence; the API key remains the selected input.
  assert.deepEqual(officialAnthropicClientOptions({
    ANTHROPIC_API_KEY: 'reference-key', // awaken-allow: secret
    ANTHROPIC_BASE_URL: 'https://gateway.invalid',
  }), {
    apiKey: 'reference-key', // awaken-allow: secret
    baseURL: 'https://api.anthropic.com',
  });
});
