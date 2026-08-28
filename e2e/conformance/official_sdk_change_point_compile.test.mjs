import assert from 'node:assert/strict';
import test from 'node:test';

import {
  officialSdkChangePointFixture,
  pathBelongsToPackage,
} from './official_sdk_change_point_compile.mjs';

test('candidate compilation cannot resolve declarations from a sibling or parent SDK', () => {
  // Cause/effect table: C1 resolution is inside the explicit package root;
  // C2 resolution is a prefix-collision sibling; C3 resolution escapes to a
  // parent install; C4 resolution is the package directory itself. Only C1 is
  // admissible. This prevents a missing candidate declaration from silently
  // compiling against the workspace's pinned SDK and producing a false green.
  const root = '/candidate/node_modules/@anthropic-ai/sdk';
  assert.equal(pathBelongsToPackage(root, `${root}/index.d.mts`), true, 'C1');
  assert.equal(pathBelongsToPackage(root, `${root}-old/index.d.mts`), false, 'C2');
  assert.equal(
    pathBelongsToPackage(root, '/candidate/node_modules/@anthropic-ai/sdk/index.d.mts'),
    true,
    'C1 normalized',
  );
  assert.equal(
    pathBelongsToPackage(root, '/candidate/node_modules/@anthropic-ai/index.d.mts'),
    false,
    'C3',
  );
  assert.equal(pathBelongsToPackage(root, root), false, 'C4');
});

test('candidate compile fixtures are projection-complete and contain no escape hatch', () => {
  // Cause/effect graph: C1 historical capability-bearing operations; C2
  // query-only post-GA operations; C3 parseUnverified exists. Effects: E1 Beta
  // fields/parameters only, E2 GA fields/parameters only, E3 helper invocation;
  // E4 no any/cast/ts-ignore. Rules T1 C1->E1+E4; T2 C2+C3->E2+E3+E4.
  const beta = officialSdkChangePointFixture({
    filesProjection: 'beta', skillsProjection: 'beta', parseUnverified: false,
  });
  const ga = officialSdkChangePointFixture({
    filesProjection: 'ga', skillsProjection: 'ga', parseUnverified: true,
  });
  assert.match(beta, /fileMetadata\.scope/u, 'T1/E1');
  assert.match(beta, /skill\.latest_version;/u, 'T1/E1');
  assert.doesNotMatch(beta, /expires_in_seconds|parseUnverified/u, 'T1/E1');
  assert.match(ga, /expires_in_seconds: 3_600/u, 'T2/E2');
  assert.match(ga, /skill\.latest_version_id;/u, 'T2/E2');
  assert.match(ga, /parseUnverified/u, 'T2/E3');
  for (const fixture of [beta, ga]) {
    assert.doesNotMatch(fixture, /\bany\b|\sas\s|@ts-/u, 'E4');
  }
  assert.throws(
    () => officialSdkChangePointFixture({
      filesProjection: 'unknown', skillsProjection: 'ga', parseUnverified: false,
    }),
    /unsupported Files projection/u,
  );
});
