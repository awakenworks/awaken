import assert from 'node:assert/strict';
import fs from 'node:fs';
import { test } from 'node:test';
import {
  workdirWorkerManifestFixture,
  WORKER_PROVIDER_CREDENTIAL_CAPABILITY,
  WORKER_INHERITED_TOOLSETS_CAPABILITY,
} from './fixtures/worker_manifest_fixture.mjs';

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

test('the Provider capability fixture follows the exact Rust credential codec', () => {
  // R1 the fixture's version matches Rust and its unsealed profile is explicit
  // -> domain E2Es can enter admission. R2 a codec bump or omitted narrowing
  // field -> fail here before starting any service. Never auto-promote peers.
  const source = fs.readFileSync(new URL('../crates/contract/awaken-credential-contract/src/realization_capabilities.rs', import.meta.url), 'utf8');
  const version = source.match(/pub const CREDENTIAL_REALIZATION_CAPABILITY_PREFIX: &str =\s*"([^"]+)";/);
  assert.ok(version, 'the Rust capability namespace remains authoritative');
  assert.ok(WORKER_PROVIDER_CREDENTIAL_CAPABILITY.startsWith(version[1]), 'review the peer for the current codec');
  const profile = JSON.parse(WORKER_PROVIDER_CREDENTIAL_CAPABILITY.slice(version[1].length));
  assert.equal(profile.envelope_required, false);
  assert.equal(profile.recipient_bound_envelopes, false);
});

test('the external Worker fixture names the frozen child-toolset format exactly', () => {
  // Causes: C1 a fresh root carries inherited toolsets; C2 the simulated
  // Workdir Worker opts into their format. Effect: E1 placement may reach the
  // HTTP claim/commit tests only when C2 equals Rust's canonical capability.
  // Rule C1+C2=>E1; a renamed format fails here before either transport E2E.
  const source = fs.readFileSync(new URL('../crates/contract/awaken-runtime-contract/src/activation.rs', import.meta.url), 'utf8');
  const capability = source.match(/pub const INHERITED_TOOLSETS_CAPABILITY: &str = "([^"]+)";/);
  assert.ok(capability, 'the Rust inherited-toolset capability remains authoritative');
  assert.equal(WORKER_INHERITED_TOOLSETS_CAPABILITY, capability[1]);
});
