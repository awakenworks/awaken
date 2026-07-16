// Beta-version drift sentinel (fx oracle, offline).
//
// The Managed wire is pinned to a dated beta (`managed-agents-2026-04-01`). Three
// places must agree: awaken's Rust `MANAGED_BETA` const, the e2e harness `BETAS`,
// and the identifier the installed SDK ships. If a future SDK bump changes the
// date, this goes red — forcing a human to re-pin deliberately rather than drift
// silently. Purely static: no server, no network.
//
// Run: (from e2e/)  node conformance/gate_beta_sentinel_e2e.mjs

import assert from 'node:assert/strict';
import { rustBeta, harnessBetas, sdkManagedBetas } from './catalog.mjs';

async function main() {
  try {
    const rust = rustBeta();
    const harness = harnessBetas();
    const sdk = sdkManagedBetas();

    console.log(`  Rust MANAGED_BETA = ${rust}`);
    console.log(`  harness BETAS     = ${JSON.stringify(harness)}`);
    console.log(`  SDK managed betas = ${JSON.stringify(sdk)}`);

    // The harness sends exactly the pinned beta.
    assert.deepEqual(harness, [rust], 'harness BETAS must be exactly [MANAGED_BETA]');
    // The SDK must know this beta identifier.
    assert.ok(
      sdk.includes(rust),
      `the installed SDK does not declare ${rust} (it ships ${JSON.stringify(sdk)}) — re-pin MANAGED_BETA`,
    );

    console.log('GATE PASS: MANAGED_BETA agrees across Rust, harness, and the installed SDK.');
    process.exitCode = 0;
  } catch (err) {
    console.error('GATE FAIL:', err.message || err);
    process.exitCode = 1;
  }
}

main();
