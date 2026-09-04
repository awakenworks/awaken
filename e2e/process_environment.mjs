// One E2E process-boundary owner for the production ambient-provider-key guard.
// Product processes reject inherited API-key variables; deterministic fixtures
// may add an explicit fake key only after this filter at the exact child edge.
export function withoutAmbientProviderKeys(environment) {
  return Object.fromEntries(Object.entries(environment).filter(([name]) => {
    const normalized = name.toUpperCase();
    return normalized !== 'API_KEY' && !normalized.endsWith('_API_KEY');
  }));
}
