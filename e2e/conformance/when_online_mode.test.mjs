import assert from 'node:assert/strict';
import test from 'node:test';
import { whenOnlineMode } from './when_online_mode.mjs';

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
