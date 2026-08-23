import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import test from 'node:test';
import {
  rustInboundTypes,
  rustOutboundTypes,
  rustPreviewTypes,
  sdkEventTypes,
  sdkVersionBinding,
} from './catalog.mjs';
import {
  EVENT_BEHAVIOR_RULES,
  eventBehaviorOwners,
} from './managed_event_behavior_manifest.mjs';

const REPO = resolve(import.meta.dirname, '..', '..');

function ownedTestBlock(owner) {
  const source = readFileSync(resolve(REPO, owner.owner), 'utf8');
  const signature = new RegExp(`(?:async\\s+)?fn\\s+${owner.test}\\s*\\(`);
  const match = signature.exec(source);
  assert.ok(match, `${owner.type}: missing behavior test ${owner.test} in ${owner.owner}`);
  const nextTest = /\n#\[(?:tokio::)?test\]/g;
  nextTest.lastIndex = match.index + match[0].length;
  const next = nextTest.exec(source);
  const prefix = source.slice(0, match.index);
  const closingLine = /^\s*}\s*$/gm;
  let designStart = 0;
  for (const closing of prefix.matchAll(closingLine)) designStart = closing.index + closing[0].length;
  return {
    design: source.slice(designStart, match.index),
    body: source.slice(match.index, next?.index ?? source.length),
  };
}

test('every official Managed event has one causal behavior owner', () => {
  // Cause/effect graph: C1=the installed SDK is the exact pinned version;
  // C2=SDK persisted/inbound/preview types equal their Rust wire enums;
  // C3=every SDK event matches exactly one behavior-owner rule; C4=that named
  // test exists, asserts the event type, and carries Cause, Effect, Constraint,
  // and Decision evidence beside the executable oracle. Effects: E1=all 35
  // persisted and two preview events remain wire-compatible and traceable to
  // one causal behavior test; E2=SDK additions, Rust drift, missing/overlapping
  // owners, stale anchors, or evidence-free tests fail deterministically.
  // Constraint K1: the SDK extractor is the event vocabulary authority; these
  // regex rules own only test traceability and may not become another catalog.
  // Decision table: R1 C1+C2+C3+C4=>E1; R2 !C1||!C2=>E2 at the catalog edge;
  // R3 C1+C2+(!C3||!C4)=>E2 at the behavior-ownership edge.
  const version = sdkVersionBinding();
  assert.equal(version.installed, version.pinned, 'R1/C1');

  const sdk = sdkEventTypes();
  assert.deepEqual(rustOutboundTypes(), sdk.outbound, 'R1/C2 persisted catalog');
  assert.deepEqual(rustInboundTypes(), sdk.inbound, 'R1/C2 inbound catalog');
  assert.deepEqual(rustPreviewTypes(), sdk.preview, 'R1/C2 preview catalog');

  const owners = eventBehaviorOwners([...sdk.outbound, ...sdk.preview]);
  assert.equal(owners.length, sdk.outbound.length + sdk.preview.length, 'R1/C3');
  for (const owner of owners) {
    const block = ownedTestBlock(owner);
    const evidence = block.design + block.body.slice(0, 3_000);
    assert.ok(block.body.includes(owner.type), `${owner.type}: owner does not assert its wire type`);
    assert.match(
      evidence,
      /Cause(?:\/effect)?|\bC\d+/i,
      `${owner.type}: missing causes`,
    );
    assert.match(
      evidence,
      /Effect|\bE\d+/i,
      `${owner.type}: missing effects`,
    );
    assert.match(
      evidence,
      /Constraint|\bK\d+/i,
      `${owner.type}: missing constraints`,
    );
    assert.match(
      evidence,
      /Decision|\|\s*Rule\s*\|/i,
      `${owner.type}: missing decision rule`,
    );
  }
});

test('event ownership rejects missing and overlapping rules', () => {
  // Cause/effect decision table: C1=no rule matches; C2=two rules match;
  // E1=the gate rejects an unowned event; E2=the gate rejects parallel test
  // authority. Rules: N1 C1&&!C2=>E1; N2 !C1&&C2=>E2. K1 no waiver or
  // first-match ordering may hide either catalog defect.
  assert.throws(() => eventBehaviorOwners(['future.event'], []), /found 0/, 'N1/E1');
  assert.throws(
    () => eventBehaviorOwners(['user.message'], [
      ...EVENT_BEHAVIOR_RULES,
      { matches: /^user\./, owner: 'parallel', test: 'parallel' },
    ]),
    /found 2/,
    'N2/E2',
  );
});
