import { describe, it, expect, vi, afterEach } from "vitest";
import { formatRelativeTime } from "./format-time";

const NOW = new Date("2026-05-01T12:00:00.000Z").getTime();

describe("formatRelativeTime", () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  function at(ms: number) {
    vi.setSystemTime(NOW);
    return formatRelativeTime(NOW - ms);
  }

  it('returns "—" for undefined', () => {
    expect(formatRelativeTime(undefined)).toBe("—");
  });

  it('returns "—" for null', () => {
    expect(formatRelativeTime(null)).toBe("—");
  });

  it('returns "just now" for 0 seconds ago', () => {
    vi.setSystemTime(NOW);
    expect(formatRelativeTime(NOW)).toBe("just now");
  });

  it('returns "just now" for 30 seconds ago', () => {
    expect(at(30_000)).toBe("just now");
  });

  it('returns "just now" for 59 seconds ago', () => {
    expect(at(59_000)).toBe("just now");
  });

  it('returns "Xm ago" for 1 minute ago', () => {
    expect(at(60_000)).toBe("1m ago");
  });

  it('returns "Xm ago" for 45 minutes ago', () => {
    expect(at(45 * 60_000)).toBe("45m ago");
  });

  it('returns "Xm ago" for 59 minutes ago', () => {
    expect(at(59 * 60_000)).toBe("59m ago");
  });

  it('returns "Xh ago" for 1 hour ago', () => {
    expect(at(60 * 60_000)).toBe("1h ago");
  });

  it('returns "Xh ago" for 2 hours ago', () => {
    expect(at(2 * 60 * 60_000)).toBe("2h ago");
  });

  it('returns "Xh ago" for 23 hours ago', () => {
    expect(at(23 * 60 * 60_000)).toBe("23h ago");
  });

  it('returns "Xd ago" for 1 day ago', () => {
    expect(at(24 * 60 * 60_000)).toBe("1d ago");
  });

  it('returns "Xd ago" for 5 days ago', () => {
    expect(at(5 * 24 * 60 * 60_000)).toBe("5d ago");
  });

  it('returns "Xd ago" for 6 days ago', () => {
    expect(at(6 * 24 * 60 * 60_000)).toBe("6d ago");
  });

  it("returns absolute date for 7 days ago", () => {
    const result = at(7 * 24 * 60 * 60_000);
    expect(result).toMatch(/\w+ \d+, \d{4}/);
  });

  it("returns absolute date for much older timestamps", () => {
    vi.setSystemTime(NOW);
    // Apr 20 is 11 days before May 1, so it falls into the absolute-date branch.
    const result = formatRelativeTime(new Date("2026-04-20T00:00:00.000Z").getTime());
    expect(result).toBe("Apr 20, 2026");
  });

  it("auto-detects Unix seconds and treats them as ms × 1000", () => {
    vi.setSystemTime(NOW);
    // updated_at on the wire is Unix seconds — passing it directly should
    // still produce a relative string, not "Jan 1970".
    const fiveMinAgoSec = Math.floor(NOW / 1000) - 5 * 60;
    expect(formatRelativeTime(fiveMinAgoSec)).toBe("5m ago");
  });
});
