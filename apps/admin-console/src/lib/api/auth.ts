import { BACKEND_URL, ConfigApiError, fetchJson } from "./http";

export interface SessionInfo {
  subject: string;
}

export interface AuthCapabilities {
  oauth_enabled: boolean;
  login_url: string;
}

export const authApi = {
  /**
   * GET /v1/auth/capabilities — public, no auth required.
   * Returns 200 with {oauth_enabled, login_url} when OAuth is configured,
   * or throws ConfigApiError(404) when the OAuth routes are not mounted.
   */
  capabilities: () =>
    fetchJson<AuthCapabilities>(`${BACKEND_URL}/v1/auth/capabilities`),

  /**
   * Return info about the current session (subject).
   * Throws ConfigApiError with status 401 when not authenticated.
   */
  me: () => fetchJson<SessionInfo>(`${BACKEND_URL}/v1/auth/me`),

  /**
   * Build the login URL with an optional `return_to` destination.
   * The browser navigates directly to this URL — it is NOT fetched via XHR.
   */
  loginUrl: (returnTo?: string): string => {
    const base = `${BACKEND_URL}/v1/auth/login`;
    if (!returnTo) return base;
    return `${base}?return_to=${encodeURIComponent(returnTo)}`;
  },

  /**
   * POST /v1/auth/logout — clears the server-side session.
   */
  logout: () =>
    fetchJson<void>(`${BACKEND_URL}/v1/auth/logout`, { method: "POST" }),
};

/**
 * Extract an `access_token` value from a URL fragment, e.g. after the
 * OAuth callback redirects to `/#access_token=<session_id>`.
 */
export function extractAccessTokenFromFragment(fragment: string): string | null {
  const stripped = fragment.startsWith("#") ? fragment.slice(1) : fragment;
  const params = new URLSearchParams(stripped);
  const token = params.get("access_token");
  return token && token.trim().length > 0 ? token.trim() : null;
}

/**
 * Extract an `oauth_error` from a URL fragment, set by the server when the
 * IdP returns an error.
 */
export function extractOAuthErrorFromFragment(fragment: string): string | null {
  const stripped = fragment.startsWith("#") ? fragment.slice(1) : fragment;
  const params = new URLSearchParams(stripped);
  return params.get("oauth_error") ?? null;
}

export { ConfigApiError };
