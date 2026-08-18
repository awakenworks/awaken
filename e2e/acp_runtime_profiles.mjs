import assert from 'node:assert/strict';

export const ACP_RUNTIME_IDS = Object.freeze(['claude', 'codex', 'gemini', 'opencode', 'hermes']);
export const ACP_DEFAULT_MEMORY_RUNTIMES = Object.freeze(['opencode', 'claude', 'hermes']);
export const ACP_RUNTIME_VERSION_SPECS = Object.freeze({
  claude: Object.freeze({ command: ['claude', '--version'], minimum: [2, 1, 221] }),
  codex: Object.freeze({ command: ['codex', '--version'], minimum: [0, 146, 0] }),
  gemini: Object.freeze({ command: ['gemini', '--version'], minimum: [0, 53, 1] }),
  opencode: Object.freeze({ command: ['opencode', '--version'], minimum: [1, 18, 12] }),
  hermes: Object.freeze({ command: ['hermes', 'version'], minimum: [0, 19, 0] }),
});

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
    const profile = directProfile(runtime, env) ?? kimiProfile(runtime, kimi);
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

export const ACP_RUNTIME_ENV_KEYS = Object.freeze([
  'ANTHROPIC_BASE_URL',
  'ANTHROPIC_API_KEY',
  'ANTHROPIC_MODEL',
  'OPENAI_BASE_URL',
  'OPENAI_API_KEY',
  'OPENAI_MODEL',
  'OPENCODE_CONFIG_CONTENT',
  'KIMI_BASE_URL',
  'KIMI_API_KEY',
  'HERMES_MODEL',
  'GEMINI_API_KEY',
  'GEMINI_MODEL',
  'GOOGLE_GEMINI_BASE_URL',
]);

export function applyAcpRuntimeProfile(profile, env = process.env) {
  for (const key of ACP_RUNTIME_ENV_KEYS) delete env[key];
  Object.assign(env, profile.env, {
    AWAKEN_ACP_CLI: profile.runtime,
    AWAKEN_MODEL: profile.model,
  });
}
