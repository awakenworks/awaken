const EXACT_VERSION = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

function isExactVersion(value) {
  return typeof value === 'string' && EXACT_VERSION.test(value);
}

export function latestCanaryPlan(oracle, latest, installed, releasePolicy = undefined) {
  if (!isExactVersion(oracle)) {
    throw new Error(
      `the generated Managed SDK current oracle must be exact; got ${JSON.stringify(oracle)}`,
    );
  }
  if (!isExactVersion(latest)) {
    throw new Error(`npm returned an invalid @anthropic-ai/sdk version: ${JSON.stringify(latest)}`);
  }
  if (installed !== oracle) {
    throw new Error(
      `installed Managed SDK current anchor ${JSON.stringify(installed)} does not match oracle ${oracle}`,
    );
  }
  if (latest !== oracle) {
    const publishedAt = Date.parse(releasePolicy?.latestPublishedAt);
    const now = releasePolicy?.now;
    const minimumReleaseAgeMinutes = releasePolicy?.minimumReleaseAgeMinutes;
    if (!Number.isFinite(publishedAt)
      || !Number.isFinite(now)
      || !Number.isFinite(minimumReleaseAgeMinutes)
      || minimumReleaseAgeMinutes <= 0) {
      throw new Error('registry drift requires one valid minimum-release-age policy');
    }
    const eligibleAt = publishedAt + minimumReleaseAgeMinutes * 60_000;
    if (now < eligibleAt) {
      return Object.freeze({
        oracle,
        latest,
        installed,
        quarantinedUntil: new Date(eligibleAt).toISOString(),
      });
    }
    throw new Error(
      `registry latest @anthropic-ai/sdk ${JSON.stringify(latest)} does not match generated `
      + `current oracle ${oracle}; update the current anchor and regenerate the Managed SDK oracle`,
    );
  }
  return Object.freeze({ oracle, latest, installed });
}
