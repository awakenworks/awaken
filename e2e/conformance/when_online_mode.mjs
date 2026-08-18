export function whenOnlineMode(env) {
  const required = env.AWAKEN_WHEN_ONLINE_REQUIRED === '1';
  const optedIn = env.AWAKEN_WHEN_ONLINE === '1';
  const hasKey = typeof env.ANTHROPIC_API_KEY === 'string' && env.ANTHROPIC_API_KEY.length > 0;
  if (required && !hasKey) {
    return Object.freeze({ run: false, error: 'ANTHROPIC_API_KEY is required by the release gate' });
  }
  return Object.freeze({ run: hasKey && (required || optedIn), error: null });
}
