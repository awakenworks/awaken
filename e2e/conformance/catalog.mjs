// Shared source-of-truth extractors for the Managed Agents conformance gates.
//
// The oracle is the *installed* official SDK (`@anthropic-ai/sdk`, pinned in
// e2e/package.json) — its vendored `.d.ts` is the authoritative event catalog and
// beta identifier. The subject-under-test is awaken's Rust wire vocabulary, read
// straight from source (the `#[serde(rename=…)]` tags on the event enums and the
// `MANAGED_BETA` const). No server, no network — pure static comparison.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const read = (rel) => readFileSync(path.join(REPO, rel), 'utf8');

const SESSION_RS = 'crates/server/awaken-protocol-managed/src/types/session.rs';
const BRIDGE_RS = 'crates/server/awaken-managed-bridge/src/lib.rs';
const EVENTS_DTS = 'e2e/node_modules/@anthropic-ai/sdk/resources/beta/sessions/events.d.ts';
// `session.updated` / `system.message` events are declared here and re-exported
// into the event unions, so the catalog scan must read this file too.
const SESSIONS_DTS = 'e2e/node_modules/@anthropic-ai/sdk/resources/beta/sessions/sessions.d.ts';
const BETA_DTS = 'e2e/node_modules/@anthropic-ai/sdk/resources/beta/beta.d.ts';
const MANAGED_E2E = 'e2e/managed_e2e.mjs';

// -- SDK oracle -------------------------------------------------------------

// Every event `type` string literal the installed SDK declares, split by family.
// The SDK's outbound stream union also echoes `user.*` events; `OutboundKind` does
// not model those (awaken never re-emits a user event), so the outbound catalog we
// compare against is the agent./session./span. families. Inbound = user./system.
export function sdkEventTypes() {
  const text = read(EVENTS_DTS) + '\n' + read(SESSIONS_DTS);
  const all = new Set();
  for (const m of text.matchAll(/type:\s*'([a-z_]+(?:\.[a-z_]+)+)'/g)) all.add(m[1]);
  const types = [...all];
  return {
    outbound: types.filter((t) => /^(agent|session|span)\./.test(t)).sort(),
    inbound: types.filter((t) => /^(user|system)\./.test(t)).sort(),
  };
}

// The managed beta identifiers the SDK ships (e.g. `managed-agents-2026-04-01`).
export function sdkManagedBetas() {
  const found = new Set();
  for (const rel of [BETA_DTS, EVENTS_DTS]) {
    for (const m of read(rel).matchAll(/managed-agents-\d{4}-\d{2}-\d{2}/g)) found.add(m[0]);
  }
  return [...found].sort();
}

// -- Rust subject-under-test (parsed from source) ---------------------------

// The `#[serde(rename="…")]` wire tags inside a named enum body. Anchored on the
// enum declaration and the `impl` that follows it (the file's stable structure).
function rustEnumRenames(src, enumName) {
  const start = src.indexOf(`pub enum ${enumName} {`);
  if (start < 0) throw new Error(`enum ${enumName} not found in session.rs`);
  const end = src.indexOf(`impl ${enumName}`, start);
  const body = src.slice(start, end < 0 ? undefined : end);
  return [...body.matchAll(/#\[serde\(rename\s*=\s*"([^"]+)"\)\]/g)].map((m) => m[1]).sort();
}

export function rustOutboundTypes() {
  return rustEnumRenames(read(SESSION_RS), 'OutboundKind');
}
export function rustInboundTypes() {
  return rustEnumRenames(read(SESSION_RS), 'InboundEvent');
}

export function rustBeta() {
  const m = read(BRIDGE_RS).match(/MANAGED_BETA:\s*&str\s*=\s*"([^"]+)"/);
  if (!m) throw new Error('MANAGED_BETA const not found in awaken-managed-bridge');
  return m[1];
}

export function harnessBetas() {
  const m = read(MANAGED_E2E).match(/BETAS\s*=\s*\[([^\]]*)\]/);
  if (!m) throw new Error('BETAS array not found in managed_e2e.mjs');
  return [...m[1].matchAll(/'([^']+)'/g)].map((x) => x[1]);
}

// Known catalog divergences that are accepted (empty today). Each entry: the wire
// type plus why it is tolerated. A divergence NOT listed here fails the gate.
export const CATALOG_WAIVERS = {
  // SDK outbound types awaken deliberately does not model in OutboundKind.
  outboundMissing: {},
  // OutboundKind types the SDK does not declare (should always be empty).
  outboundExtra: {},
};
