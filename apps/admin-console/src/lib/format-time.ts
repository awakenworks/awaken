/**
 * Format an epoch-millisecond timestamp as a human-readable relative string.
 *
 * - Within the last 60 seconds: "just now"
 * - Within the last 60 minutes: "Xm ago"
 * - Within the last 24 hours: "Xh ago"
 * - Within the last 7 days: "Xd ago"
 * - Older: locale date string e.g. "Apr 27, 2026"
 *
 * Returns "—" when the timestamp is undefined or null.
 */
export function formatRelativeTime(ms: number | undefined | null): string {
  if (ms === undefined || ms === null) {
    return "—";
  }

  const now = Date.now();
  const diffMs = now - ms;
  const diffSecs = Math.floor(diffMs / 1000);

  if (diffSecs < 60) {
    return "just now";
  }

  const diffMins = Math.floor(diffSecs / 60);
  if (diffMins < 60) {
    return `${diffMins}m ago`;
  }

  const diffHours = Math.floor(diffMins / 60);
  if (diffHours < 24) {
    return `${diffHours}h ago`;
  }

  const diffDays = Math.floor(diffHours / 24);
  if (diffDays < 7) {
    return `${diffDays}d ago`;
  }

  return new Date(ms).toLocaleDateString("en-US", {
    year: "numeric",
    month: "short",
    day: "numeric",
  });
}
