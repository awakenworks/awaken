const EXACT_VERSION = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

function isExactVersion(value) {
  return typeof value === 'string' && EXACT_VERSION.test(value);
}

export function latestCanaryPlan(oracle, latest, installed) {
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
    throw new Error(
      `registry latest @anthropic-ai/sdk ${JSON.stringify(latest)} does not match generated `
      + `current oracle ${oracle}; update the current anchor and regenerate the Managed SDK oracle`,
    );
  }
  return Object.freeze({ oracle, latest, installed });
}
