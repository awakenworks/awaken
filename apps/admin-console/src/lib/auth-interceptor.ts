/// Resolve to a fresh token to retry the request, or `null` to give up.
export type UnauthorizedHandler = () => Promise<string | null>;

let handler: UnauthorizedHandler | null = null;
let inFlight: Promise<string | null> | null = null;

/// Install or replace the global handler invoked when an admin request returns 401.
/// Returns a disposer that uninstalls the handler if it is still the active one.
export function setUnauthorizedHandler(next: UnauthorizedHandler | null): () => void {
  handler = next;
  return () => {
    if (handler === next) {
      handler = null;
    }
  };
}

/// Whether a handler is installed.
export function hasUnauthorizedHandler(): boolean {
  return handler !== null;
}

/// Invoke the registered handler, deduplicating concurrent calls so that a
/// burst of 401s only opens a single token prompt.
export async function requestUnauthorizedRetry(): Promise<string | null> {
  if (!handler) {
    return null;
  }
  if (!inFlight) {
    const current = handler;
    inFlight = current()
      .catch(() => null)
      .finally(() => {
        inFlight = null;
      });
  }
  return inFlight;
}

/// Test-only helper.
export function __resetAuthInterceptorForTesting(): void {
  handler = null;
  inFlight = null;
}
