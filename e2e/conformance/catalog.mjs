// Shared source-of-truth extractors for the Managed Agents conformance gates.
//
// The sole SDK authority is the generated `@awaken/managed-sdk-oracle` current
// anchor. E2E's installed `@anthropic-ai/sdk` is only an executable mirror and
// must match that anchor exactly. The subject-under-test is awaken's Rust wire
// vocabulary, read straight from source. No server or network is involved.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const read = (rel) => readFileSync(path.join(REPO, rel), 'utf8');

const SESSION_RS = 'crates/server/awaken-protocol-managed/src/types/session.rs';
const MANAGED_HEADERS_RS = 'crates/server/awaken-protocol-managed/src/common/headers.rs';
const SDK_ORACLE_JSON = 'contracts/anthropic-managed/upstream-oracle.generated.json';
const MANAGED_E2E = 'e2e/managed_e2e.mjs';
const E2E_PACKAGE_JSON = 'e2e/package.json';
const SDK_PACKAGE_JSON = 'e2e/node_modules/@anthropic-ai/sdk/package.json';
const sdkOracle = JSON.parse(read(SDK_ORACLE_JSON));

// -- SDK oracle -------------------------------------------------------------

// Generated from every current-SDK event `type` literal by the oracle package.
export function sdkEventTypes() {
  return structuredClone(sdkOracle.current.wire_contract.events);
}

// A catalog result is evidence for the pinned SDK only when the declarations
// being read really belong to that exact package version. Keeping this check in
// the extractor prevents every consumer from inventing its own version policy.
export function sdkVersionBinding() {
  const oracle = sdkOracle.current.version;
  const pinned = JSON.parse(read(E2E_PACKAGE_JSON)).dependencies['@anthropic-ai/sdk'];
  const installed = JSON.parse(read(SDK_PACKAGE_JSON)).version;
  if (pinned !== oracle || installed !== oracle) {
    throw new Error(
      `E2E SDK mirror pinned=${pinned} installed=${installed} does not match current oracle ${oracle}; update the oracle anchor and run npm ci in e2e`,
    );
  }
  return Object.freeze({ oracle, pinned, installed });
}

// The managed beta identifiers the SDK ships (e.g. `managed-agents-2026-04-01`).
export function sdkManagedBetas() {
  return [...sdkOracle.current.wire_contract.managed_betas];
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
