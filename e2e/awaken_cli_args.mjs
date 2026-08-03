// Automated processes must never inherit the interactive CLI presentation.
// Keep this policy beside the E2E process fixtures so every production-binary
// scenario reuses one argument source instead of independently remembering the
// browser suppression switch.
export function automatedAllInOneArgs(...options) {
  return ['all-in-one', '--no-browser', ...options];
}
