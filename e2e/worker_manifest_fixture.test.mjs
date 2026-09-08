import assert from 'node:assert/strict';
import fs from 'node:fs';
import { test } from 'node:test';
import { workdirWorkerManifestFixture } from './fixtures/worker_manifest_fixture.mjs';

test('the external Worker fixture explicitly supports the current Rust execution protocol', () => {
  // Cause/effect table: C1 the Rust protocol floor; C2 this test peer's explicit
  // supported range. R1 C1 is in C2 -> registration may reach its domain test;
  // R2 a protocol bump outside C2 -> fail here, before every dependent E2E
  // aborts at registration. Never auto-upgrade a simulated peer to claim an
  // unreviewed protocol. Actual Worker transport tests still own wire admission.
  const source = fs.readFileSync(new URL('../crates/contract/awaken-worker-contract/src/manifest.rs', import.meta.url), 'utf8');
  const version = source.match(/^pub const CURRENT_RUNTIME_PROTOCOL_VERSION: u32 = ([0-9]+);$/m);
  assert.ok(version, 'the Rust execution protocol constant remains authoritative');
  const current = Number(version[1]);
  const { runtime_protocol: supported } = workdirWorkerManifestFixture({buildDigest:'compatibility-check', capabilities:[]});
  assert.ok(supported.min > 0 && supported.max < 0xffffffff, 'no omitted/ANY compatibility claim');
  assert.ok(supported.min <= current && current <= supported.max,
    `fixture supports ${supported.min}..${supported.max}; Rust requires ${current}; review the peer before updating its range`);
});
