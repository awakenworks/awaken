// One E2E process-boundary owner for the production ambient-provider-key guard.
// Product processes reject inherited API-key variables; deterministic fixtures
// may add an explicit fake key only after this filter at the exact child edge.
export function withoutAmbientProviderKeys(environment) {
  return Object.fromEntries(Object.entries(environment).filter(([name]) => {
    const normalized = name.toUpperCase();
    return normalized !== 'API_KEY' && !normalized.endsWith('_API_KEY');
  }));
}

// Independent Workers receive execution configuration explicitly through their
// own config. Test-controller database and sealing authority must not cross the
// child boundary, even when the Worker would ignore those inherited variables.
export function withoutControlAuthority(environment) {
  return Object.fromEntries(Object.entries(withoutAmbientProviderKeys(environment))
    .filter(([name]) => {
      const key = name.toUpperCase();
      return !key.startsWith('PG') && !key.includes('DATABASE')
        && !key.includes('POSTGRES') && !key.includes('SEAL_KEY')
        && !['AWAKEN_CONFIG', 'AWAKEN_CONFIG_PATH'].includes(key);
    }));
}
