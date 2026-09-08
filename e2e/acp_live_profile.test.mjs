import assert from 'node:assert/strict';
import test from 'node:test';
import { resolveAcpLiveProfile } from './acp_live_profile.mjs';

test('one profile selects the same Linux boundary for live and fixture resource missions', () => {
  // Cause/effect table: C1 platform={Linux,macOS,Windows}; C2 deployment
  // profile; C3 its exact runtime. R1 Linux TrustedHost (live or fixture) =>
  // scoped deployment AND process-produced namespace oracle. R2 Namespace or
  // Container => their existing isolation, no TrustedHost scope. R3 non-Linux
  // => unchanged native contract, never claim Linux supervision. Exhaust all
  // 8 valid profile/runtime rows on 3 platforms (24 cases). This selects test
  // configuration only; actual OS and vendor qualification requires the E2E.
  const rows = [
    ['host-login', 'codex', false, false, true],
    ['host-login', 'claude', false, false, true],
    ['host-fixture', 'codex', false, false, true],
    ['host-fixture', 'claude', false, false, true],
    ['native-fixture', 'native', true, false, false],
    ['native-host-fixture', 'native', true, true, true],
    ['container', 'codex', false, false, false],
    ['container', 'claude', false, false, false],
  ];
  for (const platform of ['linux', 'darwin', 'win32']) {
    for (const [profile, runtime, native, nativeHost, linuxScope] of rows) {
      const selected = resolveAcpLiveProfile({ profile, runtime, platform, live: '1' });
      assert.deepEqual(selected, { profile, runtime, native, nativeHost,
        scopedHost: platform === 'linux' && linuxScope }, `${platform}/${profile}/${runtime}`);
      assert.ok(Object.isFrozen(selected));
    }
  }
});

test('live opt-in and runtime compatibility fail before deployment side effects', () => {
  // C1 live={missing,0,1,true}; C2 fixture/live; C3 runtime matches profile.
  // R4 only the exact string 1 admits live execution; fixtures need no opt-in.
  // R5 runtime mismatch or unknown profile rejects, without a fallback. The
  // pure selector has no process/filesystem dependencies: tests cannot consume
  // real credentials or start a server merely to exercise rejection rules.
  for (const live of [undefined, '0', '1', 'true']) {
    for (const profile of ['host-login', 'container']) {
      const select = () => resolveAcpLiveProfile({ profile, live, platform: 'linux' });
      if (live === '1') assert.equal(select().runtime, 'codex');
      else assert.throws(select, /CODEX_ACP_LIVE=1/u);
    }
    for (const [profile, runtime] of [
      ['host-fixture', 'codex'], ['native-fixture', 'native'], ['native-host-fixture', 'native'],
    ]) assert.equal(resolveAcpLiveProfile({ profile, runtime, live, platform: 'linux' }).runtime, runtime);
  }
  for (const [profile, runtime] of [
    ['host-login', 'native'], ['container', 'native'], ['host-fixture', 'native'],
    ['native-fixture', 'codex'], ['native-host-fixture', 'claude'], ['host-login', 'unknown'],
  ]) assert.throws(() => resolveAcpLiveProfile({ profile, runtime, live: '1', platform: 'linux' }),
    /exact runtime/u);
  assert.throws(() => resolveAcpLiveProfile({ profile: 'unknown', live: '1', platform: 'linux' }),
    /CODEX_ACP_PROFILE/u);
});
