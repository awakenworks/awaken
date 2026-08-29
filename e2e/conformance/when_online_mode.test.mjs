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
  assert.match(whenOnlineMode({ AWAKEN_WHEN_ONLINE_REQUIRED: '1' }).error, /required/);
});

test('release mode runs with a credential even without the optional opt-in flag', () => {
  assert.deepEqual(
    whenOnlineMode({ AWAKEN_WHEN_ONLINE_REQUIRED: '1', ANTHROPIC_API_KEY: 'secret' }),
    { run: true, error: null },
  );
});

test('a credential alone never opts a developer into billable external work', () => {
  assert.deepEqual(whenOnlineMode({ ANTHROPIC_API_KEY: 'secret' }), { run: false, error: null });
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
