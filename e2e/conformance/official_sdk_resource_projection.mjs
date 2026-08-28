const BETA_SELECTOR = 'beta=true';

/**
 * Derive the wire projection selected by an official SDK Beta namespace from
 * the SDK's generated operations. Files and Skills kept their `beta=true`
 * transport route after GA; the endpoint capability header is therefore the
 * authoritative discriminator between the historical Beta projection and the
 * GA projection. Mixed operation signatures are rejected instead of guessed.
 */
export function officialBetaResourceProjection(operations, family) {
  const prefix = `beta.${family}.`;
  const selected = operations.filter(({ id }) => id.startsWith(prefix));
  if (selected.length === 0) {
    throw new Error(`official SDK exposes no ${prefix} operations`);
  }

  const signatures = new Set();
  for (const operation of selected) {
    if (operation.transport_query !== BETA_SELECTOR) {
      throw new Error(
        `${operation.id} must retain the exact ${BETA_SELECTOR} transport selector`,
      );
    }
    if (!Array.isArray(operation.betas) || operation.betas.length > 1) {
      throw new Error(`${operation.id} has an ambiguous endpoint capability`);
    }
    signatures.add(operation.betas[0] ?? '');
  }
  if (signatures.size !== 1) {
    throw new Error(`official SDK ${prefix} operations disagree on endpoint capability`);
  }

  const capability = [...signatures][0];
  return Object.freeze({
    projection: capability === '' ? 'ga' : 'beta',
    ...(capability === '' ? {} : { capability }),
  });
}
