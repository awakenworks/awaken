const EXACT_VERSION = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

export function latestCanaryPlan(pinned, latest) {
  if (!EXACT_VERSION.test(pinned)) {
    throw new Error(`@anthropic-ai/sdk must stay exactly pinned; got ${JSON.stringify(pinned)}`);
  }
  if (!EXACT_VERSION.test(latest)) {
    throw new Error(`npm returned an invalid @anthropic-ai/sdk version: ${JSON.stringify(latest)}`);
  }
  return Object.freeze({ pinned, latest, fetchLatest: pinned !== latest });
}
