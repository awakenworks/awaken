// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { StatCard } from "./stat-card";

afterEach(() => {
  cleanup();
});

describe("StatCard", () => {
  it("renders block layout with value above label", () => {
    render(<StatCard label="Passed" value={42} tone="success" />);
    const value = screen.getByText("42");
    expect(value.className).toContain("text-3xl");
    expect(value.className).toContain("text-tone-success");
  });

  it("renders compact layout with uppercase label above mono value", () => {
    render(<StatCard layout="compact" label="errors" value={7} sub="2.1%" tone="warn" />);
    const value = screen.getByText("7");
    expect(value.className).toContain("font-mono");
    expect(value.className).toContain("text-2xl");
    expect(value.className).toContain("text-tone-warn");
    expect(screen.getByText("2.1%")).toBeTruthy();
  });

  it("makes the compact value hero-sized when emphasis is lg", () => {
    render(<StatCard layout="compact" label="awaiting" value={3} emphasis="lg" />);
    const value = screen.getByText("3");
    expect(value.className).toContain("text-4xl");
  });

  it("disables monospace on block layout by default", () => {
    render(<StatCard label="Total" value="hello" />);
    const value = screen.getByText("hello");
    expect(value.className).not.toContain("font-mono");
  });
});
