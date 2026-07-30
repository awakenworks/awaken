// Machine-checked inventory for every public TypeScript declaration file that
// defines an Awaken-supported protocol surface. This is an audit gate, not a
// substitute for the mapped behavior E2Es: every rule points to the test file
// containing that surface's causal graph, decision table, and runtime effects.

import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const E2E = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const surfaces = [
  {
    package: '@a2a-js/sdk',
    roots: ['dist/index.d.ts', 'dist/client/index.d.ts'],
    rules: [[/^dist\//, 'a2a_e2e.mjs']],
  },
  {
    package: '@ag-ui/core',
    roots: ['dist/index.d.ts'],
    rules: [[/^dist\//, 'ag_ui_e2e.mjs']],
  },
  {
    package: '@ag-ui/client',
    roots: ['dist/index.d.ts'],
    rules: [[/^dist\//, 'ag_ui_e2e.mjs']],
  },
  {
    package: 'ai',
    roots: ['dist/index.d.ts'],
    rules: [[/^dist\//, 'ai_sdk_extra_e2e.mjs']],
  },
  {
    package: '@modelcontextprotocol/sdk',
    roots: [
      'dist/esm/types.d.ts',
      'dist/esm/client/index.d.ts',
      'dist/esm/client/streamableHttp.d.ts',
    ],
    rules: [[/^dist\/esm\//, 'mcp_official_sdk_e2e.ts']],
  },
  {
    package: '@anthropic-ai/sdk',
    roots: [
      'resources/beta/agents.d.ts',
      'resources/beta/agents/versions.d.ts',
      'resources/beta/environments.d.ts',
      'resources/beta/environments/work.d.ts',
      'resources/beta/deployments.d.ts',
      'resources/beta/deployment-runs.d.ts',
      'resources/beta/dreams.d.ts',
      'resources/beta/sessions.d.ts',
      'resources/beta/sessions/events.d.ts',
      'resources/beta/sessions/resources.d.ts',
      'resources/beta/sessions/threads.d.ts',
      'resources/beta/sessions/threads/events.d.ts',
      'resources/beta/files.d.ts',
      'resources/beta/memory-stores.d.ts',
      'resources/beta/memory-stores/memories.d.ts',
      'resources/beta/memory-stores/memory-versions.d.ts',
      'resources/beta/skills.d.ts',
      'resources/beta/skills/versions.d.ts',
      'resources/beta/vaults.d.ts',
      'resources/beta/vaults/credentials.d.ts',
      'resources/beta/user-profiles.d.ts',
    ],
    rules: [
      [
        /^resources\/beta\/agents\/versions\.d\.ts$/,
        'management_agents_e2e.mjs',
        '[SDK:resources/beta/agents/versions.d.ts]',
      ],
      [
        /^resources\/beta\/agents\/agents\.d\.ts$/,
        'management_agents_e2e.mjs',
        '[SDK:resources/beta/agents/agents.d.ts]',
      ],
      [
        /resources\/beta\/agents(?!\/(?:versions|agents)\.d\.ts$)/,
        'management_agents_e2e.mjs',
      ],
      [/resources\/beta\/environments/, 'management_environments_e2e.mjs'],
      [/resources\/beta\/deployment/, 'management_deployments_e2e.mjs'],
      [/resources\/beta\/dreams/, 'managed_dream_e2e.ts'],
      [/resources\/beta\/sessions/, 'management_sessions_family_e2e.mjs'],
      [/resources\/beta\/files/, 'management_files_models_e2e.mjs'],
      [/resources\/beta\/memory-stores/, 'management_memory_stores_e2e.mjs'],
      [/resources\/beta\/skills/, 'management_skills_e2e.mjs'],
      [/resources\/beta\/vaults/, 'management_vaults_family_e2e.mjs'],
      [/resources\/beta\/user-profiles/, 'management_user_profiles_e2e.mjs'],
    ],
    exclusions: [[
      /^(client|internal\/|core\/|lib\/|tools\/|pagination|resource|error|uploads|version|index|resources\/messages|resources\/models)/,
      'shared SDK client/runtime or non-Managed Messages/Models machinery; no Awaken Managed wire DTO',
    ]],
  },
];

const importPattern = /(?:from\s*|import\s*\(|export\s+[^;]*?from\s*)['"]([^'"]+)['"]/g;

function resolveDeclaration(from, specifier) {
  if (!specifier.startsWith('.')) return null;
  const base = resolve(dirname(from), specifier);
  const candidates = [
    base.replace(/\.js$/, '.d.ts'),
    base.replace(/\.mjs$/, '.d.mts'),
    `${base}.d.ts`,
    `${base}.d.mts`,
    /\.d\.(?:m)?ts$/.test(base) ? base : null,
    resolve(base, 'index.d.ts'),
  ].filter(Boolean);
  return candidates.find(existsSync) ?? null;
}

function declarationClosure(packageRoot, roots) {
  const pending = roots.map((root) => resolve(packageRoot, root));
  const visited = new Set();
  while (pending.length) {
    const file = pending.pop();
    assert.ok(existsSync(file), `SDK declaration root is missing: ${file}`);
    if (visited.has(file)) continue;
    visited.add(file);
    const source = readFileSync(file, 'utf8');
    for (const match of source.matchAll(importPattern)) {
      const imported = resolveDeclaration(file, match[1]);
      if (imported && imported.startsWith(packageRoot)) pending.push(imported);
    }
  }
  return [...visited].sort();
}

let files = 0;
let excluded = 0;
const mappedTests = new Set();
for (const surface of surfaces) {
  const packageRoot = resolve(E2E, 'node_modules', surface.package);
  const declarations = declarationClosure(packageRoot, surface.roots);
  assert.ok(declarations.length > 0, `${surface.package} declaration closure is empty`);
  for (const declaration of declarations) {
    const path = relative(packageRoot, declaration);
    const matches = surface.rules.filter(([pattern]) => pattern.test(path));
    if (matches.length === 0) {
      const exclusions = (surface.exclusions ?? []).filter(([pattern]) => pattern.test(path));
      assert.equal(exclusions.length, 1,
        `${surface.package}/${path} needs one behavior owner or one evidenced exclusion`);
      assert.ok(exclusions[0][1].length > 20, `${surface.package}/${path} exclusion needs a reason`);
      excluded += 1;
      continue;
    }
    assert.equal(
      matches.length,
      1,
      `${surface.package}/${path} must have exactly one behavior-test owner; got ${matches.length}`,
    );
    const test = resolve(E2E, matches[0][1]);
    assert.ok(existsSync(test), `mapped behavior test is missing: ${matches[0][1]}`);
    if (matches[0][2]) {
      const source = readFileSync(test, 'utf8');
      assert.ok(
        source.includes(matches[0][2]),
        `${surface.package}/${path} behavior owner must contain ${matches[0][2]}`,
      );
    }
    mappedTests.add(test);
    files += 1;
  }
}

for (const test of mappedTests) {
  const source = readFileSync(test, 'utf8').toLowerCase();
  assert.ok(source.includes('causal graph') || source.includes('cause graph') || source.includes('cause/effect graph'),
    `${relative(E2E, test)} must keep its causal graph beside the behavior test`);
  assert.ok(source.includes('decision table'),
    `${relative(E2E, test)} must keep its decision table beside the behavior test`);
}

console.log(
  `SDK SURFACE COVERAGE PASS: ${files} relevant declaration files -> ` +
  `${mappedTests.size} behavior E2Es; ${excluded} imported support files explicitly excluded.`,
);
