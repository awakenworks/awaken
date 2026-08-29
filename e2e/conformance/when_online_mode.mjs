const OFFICIAL_ANTHROPIC_BASE_URL = 'https://api.anthropic.com';
const REQUIRED_ONLINE_FIELDS = Object.freeze([
  'ANTHROPIC_API_KEY',
  'AWAKEN_WHEN_ONLINE_AGENT',
  'AWAKEN_WHEN_ONLINE_ENV',
]);

function isNonEmpty(value) {
  return typeof value === 'string' && value.trim().length > 0;
}

export function whenOnlineMode(env) {
  const required = env.AWAKEN_WHEN_ONLINE_REQUIRED === '1';
  const optedIn = env.AWAKEN_WHEN_ONLINE === '1';
  const requested = required || optedIn;
  const missing = REQUIRED_ONLINE_FIELDS.filter((name) => !isNonEmpty(env[name]));
  if (requested && missing.length > 0) {
    return Object.freeze({
      run: false,
      error: `official Managed online gate requires ${missing.join(', ')}`,
    });
  }
  return Object.freeze({ run: requested, error: null });
}

export function officialAnthropicClientOptions(env) {
  return Object.freeze({
    apiKey: env.ANTHROPIC_API_KEY,
    baseURL: OFFICIAL_ANTHROPIC_BASE_URL,
  });
}
