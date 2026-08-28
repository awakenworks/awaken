import { realpathSync } from 'node:fs';
import { isAbsolute, relative, resolve, sep } from 'node:path';
import ts from 'typescript';

function filesFixture(projection) {
  if (projection === 'beta') {
    return `
  const fileMetadata = await client.beta.files.upload({ file: upload });
  const filename: string = fileMetadata.filename;
  const scope = fileMetadata.scope;
  void filename; void scope;
  await client.beta.files.retrieveMetadata(fileMetadata.id);
  await client.beta.files.list({ before_id: fileMetadata.id, limit: 1 });
  await client.beta.files.download(fileMetadata.id);
  await client.beta.files.delete(fileMetadata.id);`;
  }
  if (projection === 'ga') {
    return `
  const fileMetadata = await client.beta.files.upload({ file: upload, expires_in_seconds: 3_600 });
  const filename: string = fileMetadata.filename;
  const expiresAt: string | null | undefined = fileMetadata.expires_at;
  void filename; void expiresAt;
  await client.beta.files.retrieveMetadata(fileMetadata.id);
  await client.beta.files.list({ ids: [fileMetadata.id, 'file_missing'] });
  await client.beta.files.download(fileMetadata.id);
  await client.beta.files.delete(fileMetadata.id);`;
  }
  throw new Error(`unsupported Files projection ${JSON.stringify(projection)}`);
}

function skillsFixture(projection) {
  if (projection === 'beta') {
    return `
  const skill = await client.beta.skills.create({ display_title: 'Compile', files: [upload] });
  const displayTitle: string | null = skill.display_title;
  const firstVersion: string | null = skill.latest_version;
  void displayTitle; void firstVersion;
  await client.beta.skills.retrieve(skill.id);
  await client.beta.skills.list({ limit: 1 });
  const version = await client.beta.skills.versions.create(skill.id, { files: [upload] });
  const versionReference: string = version.version;
  await client.beta.skills.versions.retrieve(versionReference, { skill_id: skill.id });
  await client.beta.skills.versions.list(skill.id, { limit: 1 });
  await client.beta.skills.versions.download(versionReference, { skill_id: skill.id });
  await client.beta.skills.versions.delete(versionReference, { skill_id: skill.id });
  await client.beta.skills.delete(skill.id);`;
  }
  if (projection === 'ga') {
    return `
  const skill = await client.beta.skills.create({ display_name: 'Compile', files: [upload] });
  const displayName: string | null = skill.display_name;
  const sourceType: string = skill.source.type;
  const firstVersion: string | null = skill.latest_version_id;
  void displayName; void sourceType; void firstVersion;
  await client.beta.skills.retrieve(skill.id);
  await client.beta.skills.list({ source: 'custom', limit: 1 });
  const version = await client.beta.skills.versions.create(skill.id, { files: [upload] });
  const versionReference: string = version.id;
  await client.beta.skills.versions.retrieve(versionReference, { skill_id: skill.id });
  await client.beta.skills.versions.list(skill.id, { limit: 1 });
  await client.beta.skills.versions.download(versionReference, { skill_id: skill.id });
  await client.beta.skills.versions.delete(versionReference, { skill_id: skill.id });
  await client.beta.skills.delete(skill.id);`;
  }
  throw new Error(`unsupported Skills projection ${JSON.stringify(projection)}`);
}

export function pathBelongsToPackage(packageRoot, resolvedModule) {
  const remainder = relative(resolve(packageRoot), resolve(resolvedModule));
  return remainder.length > 0
    && remainder !== '..'
    && !remainder.startsWith(`..${sep}`)
    && !isAbsolute(remainder);
}

export function officialSdkChangePointFixture({
  filesProjection,
  skillsProjection,
  parseUnverified,
}) {
  return `// Generated in memory by official_sdk_change_point_compile.mjs.
import Anthropic, { toFile } from '@anthropic-ai/sdk';

const client = new Anthropic({ apiKey: 'compile-only' }); // awaken-allow: secret

async function exerciseChangePoints() {
  const upload = await toFile(new Uint8Array([1]), 'fixture.txt');
${filesFixture(filesProjection)}
${skillsFixture(skillsProjection)}
  client.beta.webhooks.unwrap('{}', { headers: {
    'webhook-id': 'event_compile',
    'webhook-timestamp': '0',
    'webhook-signature': 'v1,compile',
  } });
  ${parseUnverified ? "client.beta.webhooks.parseUnverified('{}');" : ''}
}

void exerciseChangePoints();
`;
}

export function compileOfficialSdkChangePoints(packageRoot, profile) {
  // Cause/effect graph: C1 official generated request signatures select one
  // Files/Skills projection; C2 the SDK exposes a reviewed Webhook helper set;
  // C3 an exact, externally provisioned package root owns declaration lookup.
  // Effect: E1 every changed method accepts its intended strongly typed input
  // and exposes the required response fields. Decision rule T1 C1+C2+C3->E1;
  // any missing/renamed field, parameter, method, or module fails compilation.
  // Constraint: the fixture contains no any, cast, ts-ignore, or handwritten
  // declaration and exists only in the compiler host, never in product code.
  const source = officialSdkChangePointFixture(profile);
  const forbidden = [/\bany\b/u, /\sas\s/u, /@ts-/u];
  for (const pattern of forbidden) {
    if (pattern.test(source)) throw new Error(`candidate fixture contains forbidden ${pattern}`);
  }

  const packagePrefix = resolve(packageRoot, '../../..');
  const virtualFile = resolve(packagePrefix, 'awaken-managed-candidate-canary.mts');
  const options = {
    allowSyntheticDefaultImports: true,
    esModuleInterop: true,
    lib: ['lib.es2022.d.ts', 'lib.dom.d.ts', 'lib.dom.iterable.d.ts'],
    module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    noEmit: true,
    skipLibCheck: true,
    strict: true,
    target: ts.ScriptTarget.ES2022,
    types: [],
  };
  const host = ts.createCompilerHost(options);
  const defaultGetSourceFile = host.getSourceFile.bind(host);
  const defaultFileExists = host.fileExists.bind(host);
  const defaultReadFile = host.readFile.bind(host);
  host.fileExists = (filename) => filename === virtualFile || defaultFileExists(filename);
  host.readFile = (filename) => filename === virtualFile ? source : defaultReadFile(filename);
  host.getSourceFile = (filename, languageVersion, onError, shouldCreateNewSourceFile) => (
    filename === virtualFile
      ? ts.createSourceFile(filename, source, languageVersion, true, ts.ScriptKind.TS)
      : defaultGetSourceFile(filename, languageVersion, onError, shouldCreateNewSourceFile)
  );
  const resolvedSdk = ts.resolveModuleName('@anthropic-ai/sdk', virtualFile, options, host)
    .resolvedModule;
  if (!resolvedSdk) {
    throw new Error(`candidate TypeScript cannot resolve @anthropic-ai/sdk from ${packageRoot}`);
  }
  const canonicalRoot = realpathSync(packageRoot);
  const canonicalModule = realpathSync(resolvedSdk.resolvedFileName);
  if (!pathBelongsToPackage(canonicalRoot, canonicalModule)) {
    throw new Error(
      `candidate TypeScript resolved @anthropic-ai/sdk outside ${canonicalRoot}: ${canonicalModule}`,
    );
  }
  const program = ts.createProgram([virtualFile], options, host);
  const diagnostics = ts.getPreEmitDiagnostics(program);
  if (diagnostics.length > 0) {
    throw new Error(ts.formatDiagnosticsWithColorAndContext(diagnostics, {
      getCanonicalFileName: (filename) => filename,
      getCurrentDirectory: () => packagePrefix,
      getNewLine: () => '\n',
    }));
  }
}
