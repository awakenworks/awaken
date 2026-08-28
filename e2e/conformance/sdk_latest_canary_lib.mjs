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
        candidateRequired: true,
        quarantinedUntil: new Date(eligibleAt).toISOString(),
      });
    }
    return Object.freeze({
      oracle,
      latest,
      installed,
      candidateRequired: true,
      promotionRequired: true,
    });
  }
  return Object.freeze({ oracle, latest, installed });
}

export function latestCandidateQualification(
  plan,
  qualifications,
  registryIntegrity,
  candidateDependencies,
) {
  if (!plan.candidateRequired) return undefined;
  if (!Array.isArray(qualifications)) {
    throw new Error('candidate qualification catalog must be an array');
  }
  const matches = qualifications.filter(({ baseline_version, candidate_version }) => (
    baseline_version === plan.oracle && candidate_version === plan.latest
  ));
  if (matches.length !== 1) {
    throw new Error(
      `registry candidate ${plan.oracle} -> ${plan.latest} requires one exact qualification`,
    );
  }
  const qualification = matches[0];
  if (typeof qualification.module !== 'string' || qualification.module.length === 0) {
    throw new Error(`registry candidate ${plan.latest} requires one installed module alias`);
  }
  const exactDependency = `npm:@anthropic-ai/sdk@${plan.latest}`;
  if (candidateDependencies?.[qualification.module] !== exactDependency) {
    throw new Error(
      `registry candidate ${plan.latest} module must use exact dependency ${exactDependency}`,
    );
  }
  if (typeof qualification.package_integrity !== 'string'
    || !/^sha512-[A-Za-z0-9+/]+={0,2}$/u.test(qualification.package_integrity)) {
    throw new Error(`registry candidate ${plan.latest} requires one exact sha512 integrity`);
  }
  if (registryIntegrity !== qualification.package_integrity) {
    throw new Error(
      `registry candidate ${plan.latest} integrity does not match its reviewed qualification`,
    );
  }
  return qualification;
}

export async function executeLatestCanaryPlan(plan, verification) {
  if (typeof verification?.current !== 'function'
    || (plan.candidateRequired && typeof verification?.candidate !== 'function')) {
    throw new Error('latest canary plan requires every scheduled runtime verifier');
  }
  await verification.current();
  if (plan.candidateRequired) await verification.candidate();
  if (plan.promotionRequired) {
    throw new Error(
      `registry latest @anthropic-ai/sdk ${JSON.stringify(plan.latest)} does not match generated `
      + `current oracle ${plan.oracle}; candidate verification passed, update the current anchor `
      + 'and regenerate the Managed SDK oracle',
    );
  }
}
