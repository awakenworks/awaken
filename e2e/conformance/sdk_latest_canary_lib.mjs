const EXACT_VERSION = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/;

export function latestCanaryPlan(pinned, latest, installed) {
  if (!EXACT_VERSION.test(pinned)) {
    throw new Error(`@anthropic-ai/sdk must stay exactly pinned; got ${JSON.stringify(pinned)}`);
  }
  if (!EXACT_VERSION.test(latest)) {
    throw new Error(`npm returned an invalid @anthropic-ai/sdk version: ${JSON.stringify(latest)}`);
  }
  if (installed !== pinned) {
    throw new Error(`installed @anthropic-ai/sdk ${JSON.stringify(installed)} does not match pinned ${pinned}`);
  }
  return Object.freeze({ pinned, latest, installed, fetchLatest: pinned !== latest });
}
