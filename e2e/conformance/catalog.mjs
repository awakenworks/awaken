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
const MANAGED_HEADERS_RS = 'crates/server/awaken-protocol-managed/src/common/headers.rs';
const EVENTS_DTS = 'e2e/node_modules/@anthropic-ai/sdk/resources/beta/sessions/events.d.ts';
// `session.updated` / `system.message` events are declared here and re-exported
// into the event unions, so the catalog scan must read this file too.
const SESSIONS_DTS = 'e2e/node_modules/@anthropic-ai/sdk/resources/beta/sessions/sessions.d.ts';
const BETA_DTS = 'e2e/node_modules/@anthropic-ai/sdk/resources/beta/beta.d.ts';
const MANAGED_E2E = 'e2e/managed_e2e.mjs';
const E2E_PACKAGE_JSON = 'e2e/package.json';
const SDK_PACKAGE_JSON = 'e2e/node_modules/@anthropic-ai/sdk/package.json';

// -- SDK oracle -------------------------------------------------------------

// Every event `type` string literal the installed SDK declares, split by family.
// The SDK's outbound SessionEvent union includes the accepted `user.*` and
// `system.message` history events as well as agent/session/span events. The
// inbound EventParams union is the user/system subset. Keeping the outbound set
// complete catches a history/list projection that accepts an event but cannot
// serialize it back through the official union.
export function sdkEventTypes() {
  const text = read(EVENTS_DTS) + '\n' + read(SESSIONS_DTS);
  const all = new Set();
  for (const m of text.matchAll(/type:\s*'([a-z_]+(?:\.[a-z_]+)*)'/g)) all.add(m[1]);
  const types = [...all];
  return {
    outbound: types.filter((t) => /^(agent|session|span|user|system)\./.test(t)).sort(),
    inbound: types.filter((t) => /^(user|system)\./.test(t)).sort(),
    preview: types.filter((t) => /^(event_start|event_delta)$/.test(t)).sort(),
  };
}

// A catalog result is evidence for the pinned SDK only when the declarations
// being read really belong to that exact package version. Keeping this check in
// the extractor prevents every consumer from inventing its own version policy.
export function sdkVersionBinding() {
  const pinned = JSON.parse(read(E2E_PACKAGE_JSON)).dependencies['@anthropic-ai/sdk'];
  const installed = JSON.parse(read(SDK_PACKAGE_JSON)).version;
  if (installed !== pinned) {
    throw new Error(
      `installed @anthropic-ai/sdk ${installed} does not match pinned ${pinned}; run npm ci in e2e`,
    );
  }
  return Object.freeze({ pinned, installed });
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
export function rustPreviewTypes() {
  return rustEnumRenames(read(SESSION_RS), 'PreviewFrame');
}

export function rustBeta() {
  const source = read(MANAGED_HEADERS_RS);
  // Authority-extraction decision table: C1=MANAGED_BETA aliases a
  // ManagedCapability variant; C2=that variant owns one literal in beta();
  // E1=return the owned literal. Missing C1 or C2 fails closed. Reading the
  // alias and its owner avoids reintroducing a second literal solely for this
  // static gate. Rule B1=C1+C2=>E1; B2=!C1||!C2=>error.
  const owner = source.match(
    /MANAGED_BETA:\s*&str\s*=\s*ManagedCapability::([A-Za-z]+)\.beta\(\)/,
  );
  if (!owner) throw new Error('MANAGED_BETA capability owner not found in protocol-managed headers');
  const betaBody = source.match(
    /pub const fn beta\(self\)[\s\S]*?match self \{([\s\S]*?)\n\s*\}\n\s*\}/,
  );
  if (!betaBody) throw new Error('ManagedCapability beta() body not found');
  const literal = betaBody[1].match(new RegExp(`Self::${owner[1]}\\s*=>\\s*"([^"]+)"`));
  if (!literal) throw new Error(`ManagedCapability::${owner[1]} beta literal not found`);
  return literal[1];
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
