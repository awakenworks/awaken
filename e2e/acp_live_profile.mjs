import assert from 'node:assert/strict';

// Pure deployment selection for the existing live/resource runner. Catalog
// credentials and ACP negotiation remain owned by the product's discovery;
// this selector never probes, reads a login file, or installs another profile.
export function resolveAcpLiveProfile({ profile, runtime = 'codex', platform, live }) {
  const nativeHost = profile === 'native-host-fixture';
  const native = profile === 'native-fixture' || nativeHost;
  assert.ok(native ? runtime === 'native' : ['codex', 'claude'].includes(runtime),
    'select the exact runtime for this profile');
  if (!['host-fixture', 'native-fixture', 'native-host-fixture'].includes(profile) && live !== '1') {
    throw new Error('set CODEX_ACP_LIVE=1 to confirm this test may invoke the real ACP adapter');
  }
  if (!['host-login', 'host-fixture', 'native-fixture', 'native-host-fixture', 'container'].includes(profile)) {
    throw new Error('set CODEX_ACP_PROFILE=host-login, host-fixture, native-fixture, native-host-fixture or container');
  }
  const scopedHost = platform === 'linux'
    && ['host-login', 'host-fixture', 'native-host-fixture'].includes(profile);
  return Object.freeze({ profile, runtime, native, nativeHost, scopedHost });
}
