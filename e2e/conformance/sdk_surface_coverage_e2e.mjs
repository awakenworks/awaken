// Machine-checked inventory for every public TypeScript declaration file that
// defines an Awaken-supported protocol surface. This is an audit gate, not a
// substitute for the mapped behavior E2Es: every rule points to the test file
// containing that surface's causal graph, decision table, and runtime effects.

import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';
import { isDeepStrictEqual } from 'node:util';
import { fileURLToPath } from 'node:url';
import ts from 'typescript';

const E2E = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// Reviewed public-declaration fingerprints for the pinned Anthropic SDK. Each
// digest is built only from exported class/interface/type/enum AST nodes with
// comments removed. A method, DTO member, discriminated union, enum, or exported
// declaration change therefore fails this gate even when its declaration file
// remains mapped to the same behavior test.
const anthropicContractFingerprints = {
  'resources/beta/agents/agents.d.ts': { declarations: 52, sha256: 'ff7a682a06b6baff6d579bb3bd37d209476ff461d93e4906fbf937befdb1b0e8' },
  'resources/beta/agents/index.d.ts': { declarations: 2, sha256: '73d14bac25d3e3ad0972cba64046ef3963779fa7bc9e9999b8d0d898c3256105' },
  'resources/beta/agents/versions.d.ts': { declarations: 3, sha256: 'aa3b8641073fdbc5ff22fcbd5376d682a4444fe951001d8c94d67f1cb5b7ce40' },
  'resources/beta/beta.d.ts': { declarations: 15, sha256: 'd9a436bd2e80e4c829da1d98cf8887182942a5e160c8756928004e4a6ea93a42' },
  'resources/beta/deployment-runs.d.ts': { declarations: 25, sha256: 'b56255907a3a1a18b93baa58d1545e410746f0800f0eb662f995004bded745bb' },
  'resources/beta/deployments.d.ts': { declarations: 43, sha256: 'a6e4c314c50f091a92a36432c8629f870ccf56eed3c52c2bc4eb297898fa2d21' },
  'resources/beta/dreams.d.ts': { declarations: 21, sha256: '4cf7ab0f0ceaafc17730f21068e766168f34170eb52558c84221285879ad8991' },
  'resources/beta/environments/environments.d.ts': { declarations: 19, sha256: '99af8cf98c4909c4d8a1fca7ab5cc0a0355d234ab3e2ce5c76f28fc503cbd90f' },
  'resources/beta/environments/index.d.ts': { declarations: 2, sha256: '6b26b8a3fe99abb769d8e267c7cd8a91485689b95e66dd69125976ad3c46b6fd' },
  'resources/beta/environments/work.d.ts': { declarations: 21, sha256: 'e3a721aaec9ca93b7e6012d43a57b9c9d766bcb4dda47613e2c57c1ab3bc3f5c' },
  'resources/beta/files.d.ts': { declarations: 10, sha256: 'a18e4e2a661f457c7df4435093c4a82109edf88f0f3472158ce2666c202d1b4a' },
  'resources/beta/memory-stores/index.d.ts': { declarations: 3, sha256: '8f81ef1e463bd3ab357cab90c2c2b9d855f186b0b0a24053c72d42f050bfee1a' },
  'resources/beta/memory-stores/memories.d.ts': { declarations: 18, sha256: '056de11c28b29154b16cc09bcac4e9bd759332213afb4dce4211d91f1c4aa897' },
  'resources/beta/memory-stores/memory-stores.d.ts': { declarations: 10, sha256: '97118541f01e597bde00491a996326c36d4f6f551ba70dd853fa9d2369d99152' },
  'resources/beta/memory-stores/memory-versions.d.ts': { declarations: 11, sha256: '5bb1917e8cf42f9e375dedc680cfae893a29913e955e6a4f44d7da384a1bc196' },
  'resources/beta/models.d.ts': { declarations: 11, sha256: '6fbf7646c64ea1721dd434dd31493f90446608d226c24259b647f9a23a7e734c' },
  'resources/beta/sessions/events.d.ts': { declarations: 83, sha256: '23fee72b4ae55b2e45b563c077292fc8b0930281ff8c26dca12f02ec55f2ce0f' },
  'resources/beta/sessions/index.d.ts': { declarations: 4, sha256: '6fabc9fbf45faf7f568b8f647a54aebc7462d726e77c83ab713cefc6d61e7a5a' },
  'resources/beta/sessions/resources.d.ts': { declarations: 14, sha256: 'fc58966af9e7569f20544e1e85ee8275619b7729cec764ec12de8420225860ea' },
  'resources/beta/sessions/sessions.d.ts': { declarations: 42, sha256: '649f788c91b495a014f96ca5bdb3f01370d6e6a24ad389ad19cd6166a82224b6' },
  'resources/beta/sessions/threads/events.d.ts': { declarations: 4, sha256: 'bc842c73f9c7726bac99f914822ce72d78c68e35d667ee88879d95f8e2e689f6' },
  'resources/beta/sessions/threads/index.d.ts': { declarations: 2, sha256: 'cc6c7ea0be6061df8aad5ec87e1586f4e87fd1803ebb533845bfa3353fae1984' },
  'resources/beta/sessions/threads/threads.d.ts': { declarations: 10, sha256: 'e6ec9bcb58681005ef7f7de6a750257e38052554365860c95721ffb8821f2862' },
  'resources/beta/skills/index.d.ts': { declarations: 2, sha256: '4a7cf7442ab04d181fef3916e811d5dd3975de5e4f3c85aa030bc702e3b9a04f' },
  'resources/beta/skills/skills.d.ts': { declarations: 10, sha256: 'd4989056fb63156584da746b59cb428a10a888a44d84a8d96ed60b02bec2e397' },
  'resources/beta/skills/versions.d.ts': { declarations: 11, sha256: '4f169fdeca2969a38d007a760e1ccf53e42d4bc4af8a6d770f2b0d852e4908cd' },
  'resources/beta/tunnels/certificates.d.ts': { declarations: 7, sha256: '9c9184de626dfc01425048604d1d1ef5fa8889a6420c0266addf185d1563b7e1' },
  'resources/beta/tunnels/index.d.ts': { declarations: 2, sha256: 'baee2e84440c10284bbb40ae8c2869b5280d7e0f08b4961373b22f381c6d042d' },
  'resources/beta/tunnels/tunnels.d.ts': { declarations: 10, sha256: 'ee537da07d8a01eecb6d4a1771789ccd63a66321eec8b2b9717dababdfae22af' },
  'resources/beta/user-profiles.d.ts': { declarations: 10, sha256: 'cce93225c6e8206656b3e8151fba40236db4fe1be59f5397eee16a8125898f5b' },
  'resources/beta/vaults/credentials.d.ts': { declarations: 44, sha256: '67456c760fbe92888016bd8a1df302832883b243a3d361eb0c2d782a657de65e' },
  'resources/beta/vaults/index.d.ts': { declarations: 2, sha256: '60f01d986882b40ce3f8493c58a0571c55c58811e66b7218953599ef51e22656' },
  'resources/beta/vaults/vaults.d.ts': { declarations: 10, sha256: '33c512b558cd685d45a990247c5b22eba2becafc69fb869dc1e1bdde3147cc74' },
  'resources/beta/webhooks.d.ts': { declarations: 48, sha256: '9abf53b51b18eb3a5d7d4a7efd9bd63e71038e8e6731e809569b446b7a0ad1f3' },
};

function publicContractFingerprint(file) {
  const sourceText = readFileSync(file, 'utf8');
  const source = ts.createSourceFile(file, sourceText, ts.ScriptTarget.Latest, true);
  const printer = ts.createPrinter({ removeComments: true });
  const declarations = source.statements
    .filter((statement) => {
      const exported = statement.modifiers?.some(
        (modifier) => modifier.kind === ts.SyntaxKind.ExportKeyword,
      );
      return ts.isExportDeclaration(statement) || (exported && (
        ts.isClassDeclaration(statement) ||
        ts.isInterfaceDeclaration(statement) ||
        ts.isTypeAliasDeclaration(statement) ||
        ts.isEnumDeclaration(statement)
      ));
    })
    .map((statement) => printer.printNode(ts.EmitHint.Unspecified, statement, source))
    .sort();
  assert.ok(declarations.length > 0, `no exported public contracts found in ${file}`);
  return {
    declarations: declarations.length,
    sha256: createHash('sha256').update(declarations.join('\n')).digest('hex'),
  };
}

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
    // The SDK's Beta resource root is the declaration oracle. Its import
    // closure automatically brings every present and future resource family
    // into this gate; a new family therefore fails until it has one owner.
    roots: ['resources/beta/beta.d.ts'],
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
      [/resources\/beta\/models/, 'management_files_models_e2e.mjs'],
      [/resources\/beta\/memory-stores/, 'management_memory_stores_e2e.mjs'],
      [/resources\/beta\/skills/, 'management_skills_e2e.mjs'],
      [/resources\/beta\/vaults/, 'management_vaults_family_e2e.mjs'],
      [/resources\/beta\/user-profiles/, 'management_user_profiles_e2e.mjs'],
      [/resources\/beta\/tunnels/, 'management_tunnels_contract_e2e.mjs'],
      [/resources\/beta\/webhooks/, 'managed_webhooks_official_sdk_e2e.mjs'],
      [/^resources\/beta\/beta\.d\.ts$/, 'managed_contract_guard_e2e.mjs'],
    ],
    exclusions: [[
      /^(client|internal\/|core\/|lib\/|tools\/|pagination|resource|error|uploads|version|index|resources\/messages|resources\/models|resources\/beta\/messages)/,
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
let contracts = 0;
const unknownAnthropicContracts = {};
const driftedAnthropicContracts = {};
const visitedAnthropicContracts = new Set();
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
    if (surface.package === '@anthropic-ai/sdk') {
      visitedAnthropicContracts.add(path);
      const actual = publicContractFingerprint(declaration);
      const expected = anthropicContractFingerprints[path];
      if (!expected) {
        unknownAnthropicContracts[path] = actual;
      } else if (!isDeepStrictEqual(actual, expected)) {
        driftedAnthropicContracts[path] = { expected, actual };
      }
      contracts += actual.declarations;
    }
    files += 1;
  }
}

assert.deepEqual(
  unknownAnthropicContracts,
  {},
  `Anthropic public contracts need reviewed fingerprints:\n${JSON.stringify(unknownAnthropicContracts, null, 2)}`,
);

assert.deepEqual(
  driftedAnthropicContracts,
  {},
  `Anthropic public contracts drifted; review behavior before accepting fingerprints:\n${JSON.stringify(driftedAnthropicContracts, null, 2)}`,
);

assert.deepEqual(
  [...visitedAnthropicContracts].sort(),
  Object.keys(anthropicContractFingerprints).sort(),
  'every reviewed Anthropic contract fingerprint must be reachable from the installed SDK Beta root; an older or incomplete install must not silently skip newer Managed families',
);

for (const test of mappedTests) {
  const source = readFileSync(test, 'utf8').toLowerCase();
  assert.ok(source.includes('causal graph') || source.includes('cause graph') || source.includes('cause/effect graph'),
    `${relative(E2E, test)} must keep its causal graph beside the behavior test`);
  assert.ok(source.includes('decision table'),
    `${relative(E2E, test)} must keep its decision table beside the behavior test`);
}

console.log(
  `SDK SURFACE COVERAGE PASS: ${files} relevant declaration files -> ` +
  `${mappedTests.size} behavior E2Es; ${contracts} Anthropic methods/DTOs/enums fingerprinted; ` +
  `${excluded} imported support files explicitly excluded.`,
);
