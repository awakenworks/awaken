import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const repositoryRoot = fileURLToPath(new URL('..', import.meta.url));

function assertRuntimeContract(condition, message) {
  if (!condition) throw new Error(`ACP runtime contract ${message}`);
}

export function projectAcpRuntimeContract(contract) {
  assertRuntimeContract(
    contract !== null && typeof contract === 'object' && !Array.isArray(contract),
    'must be a JSON object',
  );
  assertRuntimeContract(contract.schema_version === 1, 'has an unsupported schema version');
  assertRuntimeContract(
    Array.isArray(contract.runtimes) && contract.runtimes.length > 0,
    'must contain at least one runtime row',
  );

  const ids = [];
  const seenIds = new Set();
  const versionEntries = [];
  for (const [index, runtime] of contract.runtimes.entries()) {
    assertRuntimeContract(
      runtime !== null && typeof runtime === 'object' && !Array.isArray(runtime),
      `runtime row ${index} must be an object`,
    );
    const { id, discovery } = runtime;
    assertRuntimeContract(
      typeof id === 'string' && id.trim() !== '',
      `runtime row ${index} must have a non-empty id`,
    );
    assertRuntimeContract(!seenIds.has(id), `contains duplicate runtime id ${JSON.stringify(id)}`);

    const version = discovery?.version;
    assertRuntimeContract(
      version !== null && typeof version === 'object' && !Array.isArray(version),
      `${id} must have version discovery`,
    );
    assertRuntimeContract(
      typeof version.executable === 'string' && version.executable.trim() !== '',
      `${id} must have a non-empty version executable`,
    );
    assertRuntimeContract(
      Array.isArray(version.args) && version.args.every((argument) => typeof argument === 'string'),
      `${id} version arguments must be strings`,
    );

    const minimum = discovery?.minimum_version;
    assertRuntimeContract(
      minimum !== null && typeof minimum === 'object' && !Array.isArray(minimum),
      `${id} must have a minimum version`,
    );
    const minimumTriple = ['major', 'minor', 'patch'].map((component) => minimum[component]);
    assertRuntimeContract(
      minimumTriple.every((component) => Number.isSafeInteger(component) && component >= 0),
      `${id} minimum version components must be non-negative safe integers`,
    );

    seenIds.add(id);
    ids.push(id);
    versionEntries.push([
      id,
      Object.freeze({
        command: Object.freeze([version.executable, ...version.args]),
        minimum: Object.freeze(minimumTriple),
      }),
    ]);
  }

  return Object.freeze({
    ids: Object.freeze(ids),
    versionSpecs: Object.freeze(Object.fromEntries(versionEntries)),
  });
}

function loadAcpRuntimeContract() {
  let raw;
  try {
    raw = execFileSync(
      process.env.CARGO || 'cargo',
      ['run', '--quiet', '-p', 'awaken-run-executor-acp', '--example', 'image_runtime_contract'],
      { cwd: repositoryRoot, encoding: 'utf8' },
    );
  } catch (cause) {
    throw new Error('ACP runtime contract generator failed', { cause });
  }
  try {
    return JSON.parse(raw);
  } catch (cause) {
    throw new Error('ACP runtime contract generator returned invalid JSON', { cause });
  }
}

const runtimeContract = projectAcpRuntimeContract(loadAcpRuntimeContract());

export const ACP_RUNTIME_IDS = runtimeContract.ids;
export const ACP_RUNTIME_VERSION_SPECS = runtimeContract.versionSpecs;
export const ACP_DEFAULT_MEMORY_RUNTIMES = Object.freeze(['opencode', 'claude', 'hermes']);

const present = (value) => typeof value === 'string' && value.trim() !== '';

function kimiProfile(runtime, kimi) {
  if (!kimi?.key) return null;
  const anthropicKey = kimi.anthropicKey ?? kimi.key;
  const common = {
    ANTHROPIC_BASE_URL: kimi.anthropicBase,
    ANTHROPIC_API_KEY: anthropicKey,
    ANTHROPIC_MODEL: kimi.anthropicModel,
  };
  if (runtime === 'claude') {
    return { model: kimi.anthropicModel, auth: 'process-secret', env: common };
  }
  if (runtime === 'opencode') {
    return {
      model: kimi.openaiModel,
      auth: 'process-secret',
      env: {
        ...common,
        OPENAI_BASE_URL: kimi.openaiBase,
        OPENAI_API_KEY: kimi.key,
        OPENAI_MODEL: kimi.openaiModel,
        OPENCODE_CONFIG_CONTENT: JSON.stringify({
          model: `awaken-kimi/${kimi.openaiModel}`,
          small_model: `awaken-kimi/${kimi.openaiModel}`,
          enabled_providers: ['awaken-kimi'],
          provider: {
            'awaken-kimi': {
              npm: '@ai-sdk/openai-compatible',
              name: 'Awaken Kimi Code',
              options: { baseURL: kimi.openaiBase, apiKey: '{env:OPENAI_API_KEY}' }, // awaken-allow: secret
              models: { [kimi.openaiModel]: { name: kimi.openaiModel } },
            },
          },
        }),
      },
    };
  }
  if (runtime === 'hermes') {
    return {
      model: kimi.openaiModel,
      auth: 'process-secret',
      env: {
        ...common,
        KIMI_BASE_URL: kimi.openaiBase,
        KIMI_API_KEY: kimi.key,
        HERMES_MODEL: kimi.openaiModel,
      },
    };
  }
  return null;
}

function directProfile(runtime, env) {
  if (runtime === 'codex' && present(env.OPENAI_API_KEY)) {
    return {
      model: env.OPENAI_MODEL ?? 'gpt-5.2-codex',
      auth: 'credential-artifact',
      env: {
        OPENAI_API_KEY: env.OPENAI_API_KEY,
        OPENAI_BASE_URL: env.OPENAI_BASE_URL ?? 'https://api.openai.com/v1',
        OPENAI_MODEL: env.OPENAI_MODEL ?? 'gpt-5.2-codex',
      },
    };
  }
  const geminiKey = env.GEMINI_API_KEY ?? env.GOOGLE_API_KEY;
  if (runtime === 'gemini' && present(geminiKey)) {
    return {
      model: env.GEMINI_MODEL ?? 'gemini-2.5-flash',
      auth: 'process-secret',
      env: {
        GEMINI_API_KEY: geminiKey,
        GEMINI_MODEL: env.GEMINI_MODEL ?? 'gemini-2.5-flash',
        ...(present(env.GOOGLE_GEMINI_BASE_URL)
          ? { GOOGLE_GEMINI_BASE_URL: env.GOOGLE_GEMINI_BASE_URL }
          : {}),
      },
    };
  }
  return null;
}

function runtimeProfileCandidates(runtime, env, kimi) {
  return [directProfile(runtime, env), kimiProfile(runtime, kimi)].filter(Boolean);
}

export function parseAcpRuntimes(raw) {
  const selected = (raw ?? ACP_DEFAULT_MEMORY_RUNTIMES.join(','))
    .split(',')
    .map((runtime) => runtime.trim())
    .filter(Boolean);
  const expanded = selected.includes('all') ? [...ACP_RUNTIME_IDS] : selected;
  assert.ok(expanded.length > 0, 'ACP_RUNTIMES must select at least one runtime');
  assert.equal(new Set(expanded).size, expanded.length, 'ACP_RUNTIMES must not contain duplicates');
  for (const runtime of expanded) {
    assert.ok(ACP_RUNTIME_IDS.includes(runtime), `unknown ACP runtime ${JSON.stringify(runtime)}`);
  }
  return expanded;
}

export function resolveAcpRuntimeProfiles({ runtimes, env = process.env, kimi = null }) {
  return runtimes.map((runtime) => {
    const profile = runtimeProfileCandidates(runtime, env, kimi)[0];
    assert.ok(
      profile,
      `${runtime} has no compatible real credential profile; `
        + 'Codex requires OPENAI_API_KEY, Gemini requires GEMINI_API_KEY/GOOGLE_API_KEY, '
        + 'and Claude/OpenCode/Hermes require the Kimi compatibility configuration',
    );
    return Object.freeze({ runtime, ...profile });
  });
}

export function parseVersionTriple(raw) {
  if (typeof raw !== 'string') return null;
  const match = raw.match(/(?:^|[^0-9])(\d+)\.(\d+)\.(\d+)(?![0-9])/u);
  if (!match) return null;
  const parsed = match.slice(1).map(Number);
  return parsed.every(Number.isSafeInteger) ? parsed : null;
}

export function versionAtLeast(observed, minimum) {
  for (let index = 0; index < 3; index += 1) {
    if (observed[index] > minimum[index]) return true;
    if (observed[index] < minimum[index]) return false;
  }
  return true;
}

export function assertAcpRuntimeVersions({ runtimes, probe }) {
  return runtimes.map((runtime) => {
    const spec = ACP_RUNTIME_VERSION_SPECS[runtime];
    assert.ok(spec, `unknown ACP runtime ${JSON.stringify(runtime)}`);
    const [executable, ...args] = spec.command;
    const result = probe({ runtime, executable, args });
    assert.equal(
      result?.status,
      0,
      `${runtime} version probe failed for ${spec.command.join(' ')}: ${result?.error?.message ?? result?.stderr ?? 'no output'}`,
    );
    const raw = [result.stdout, result.stderr]
      .flatMap((value) => String(value ?? '').split(/\r?\n/u))
      .map((line) => line.trim())
      .find(Boolean);
    const observed = parseVersionTriple(raw);
    assert.ok(observed, `${runtime} returned an unparseable version: ${JSON.stringify(raw)}`);
    assert.ok(
      versionAtLeast(observed, spec.minimum),
      `${runtime} ${observed.join('.')} is below the compatible minimum ${spec.minimum.join('.')}`,
    );
    return Object.freeze({
      runtime,
      version: observed.join('.'),
      minimum: spec.minimum.join('.'),
    });
  });
}

// Enumerate exact environment inputs and outputs from the same profile factories
// used above. The recording proxy captures input-only aliases such as
// GOOGLE_API_KEY; the separate Kimi fixture prevents Kimi object fields from
// being mistaken for process-environment keys.
function projectAcpRuntimeEnvKeys() {
  const inputKeys = new Set();
  const sentinel = 'awaken-profile-projection';
  const recordingEnv = new Proxy(Object.create(null), {
    get: (_target, key) => {
      if (typeof key === 'string') inputKeys.add(key);
      return undefined;
    },
  });
  for (const runtime of ACP_RUNTIME_IDS) directProfile(runtime, recordingEnv);
  const projectionEnv = new Proxy(Object.create(null), {
    get: () => sentinel,
  });
  const projectionKimi = {
    key: sentinel,
    anthropicKey: sentinel,
    anthropicBase: sentinel,
    anthropicModel: sentinel,
    openaiBase: sentinel,
    openaiModel: sentinel,
  };
  const outputKeys = ACP_RUNTIME_IDS.flatMap((runtime) => runtimeProfileCandidates(
    runtime,
    projectionEnv,
    projectionKimi,
  ).flatMap((profile) => Object.keys(profile.env)));
  return Object.freeze([...new Set([...inputKeys, ...outputKeys])]);
}

const ACP_RUNTIME_ENV_KEYS = projectAcpRuntimeEnvKeys();

export function applyAcpRuntimeProfile(profile, env = process.env) {
  for (const key of ACP_RUNTIME_ENV_KEYS) delete env[key];
  Object.assign(env, profile.env, {
    AWAKEN_ACP_CLI: profile.runtime,
    AWAKEN_MODEL: profile.model,
  });
}
