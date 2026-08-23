// Event-catalog exhaustiveness gate (fx oracle, offline).
//
// Compares the installed SDK's event `type` catalog against awaken's Rust wire
// enums (OutboundKind / InboundEvent). A divergence not listed in CATALOG_WAIVERS
// fails the gate — so a future SDK bump that adds an event, or a Rust enum that
// drifts, goes red instead of silently slipping through. Today the catalogs match
// exactly (empty waivers). Purely static: no server, no network.
//
// Run: (from e2e/)  node conformance/gate_event_catalog_e2e.mjs

import assert from 'node:assert/strict';
import {
  sdkEventTypes,
  sdkVersionBinding,
  rustOutboundTypes,
  rustInboundTypes,
  rustPreviewTypes,
  CATALOG_WAIVERS,
} from './catalog.mjs';

const diff = (a, b) => a.filter((x) => !b.includes(x));

function checkFamily(label, sdk, rust, waivers) {
  const missing = diff(sdk, rust); // in SDK, not in Rust
  const extra = diff(rust, sdk); // in Rust, not in SDK

  const unwaivedMissing = missing.filter((t) => !(t in waivers.missing));
  const unwaivedExtra = extra.filter((t) => !(t in waivers.extra));
  // A waiver that no longer corresponds to a real divergence is stale noise.
  const staleMissing = Object.keys(waivers.missing).filter((t) => !missing.includes(t));
  const staleExtra = Object.keys(waivers.extra).filter((t) => !extra.includes(t));

  console.log(`  ${label}: SDK=${sdk.length} Rust=${rust.length}` +
    ` (missing ${missing.length}, extra ${extra.length})`);

  assert.deepEqual(
    unwaivedExtra, [],
    `${label}: Rust emits type(s) the SDK does not declare: ${unwaivedExtra.join(', ')}`,
  );
  assert.deepEqual(
    unwaivedMissing, [],
    `${label}: SDK declares type(s) awaken neither models nor waives: ${unwaivedMissing.join(', ')}`,
  );
  assert.deepEqual(
    [...staleMissing, ...staleExtra], [],
    `${label}: stale waiver(s) — divergence resolved, remove them: ${[...staleMissing, ...staleExtra].join(', ')}`,
  );
}

async function main() {
  try {
    // Cause/effect graph: C1=the declarations are the exactly pinned SDK;
    // C2=each SDK family equals its Rust wire enum; C3=waivers describe only
    // current deliberate differences. Effects: E1=accept one closed catalog;
    // E2=reject stale installation, new/removed types, or stale waivers.
    // Decision rules: R1 C1+C2+C3=>E1; R2 !C1=>E2 before comparison;
    // R3 C1+(!C2||!C3)=>E2. Constraint: this gate owns vocabulary parity;
    // behavioral ownership is delegated to the adjacent manifest test.
    const version = sdkVersionBinding();
    console.log(`  SDK version: pinned=${version.pinned} installed=${version.installed}`);
    const sdk = sdkEventTypes();
    checkFamily('outbound', sdk.outbound, rustOutboundTypes(), {
      missing: CATALOG_WAIVERS.outboundMissing,
      extra: CATALOG_WAIVERS.outboundExtra,
    });
    checkFamily('inbound', sdk.inbound, rustInboundTypes(), { missing: {}, extra: {} });
    checkFamily('preview', sdk.preview, rustPreviewTypes(), { missing: {}, extra: {} });
    console.log('GATE PASS: Rust event catalog matches the installed SDK (no undocumented drift).');
    process.exitCode = 0;
  } catch (err) {
    console.error('GATE FAIL:', err.message || err);
    process.exitCode = 1;
  }
}

main();
