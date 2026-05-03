// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { Sparkline } from "./sparkline";

afterEach(() => cleanup());

describe("Sparkline", () => {
  it("renders an svg polyline when given >=2 values", () => {
    const { container } = render(<Sparkline values={[1, 4, 2, 8, 3]} />);
    const svg = container.querySelector("svg");
    const line = container.querySelector("polyline");
    expect(svg).not.toBeNull();
    expect(line).not.toBeNull();
    const pts = line!.getAttribute("points") ?? "";
    expect(pts.split(" ")).toHaveLength(5);
  });

  it("falls back to a flat hairline when values.length < 2", () => {
    const { container } = render(<Sparkline values={[42]} />);
    expect(container.querySelector("svg")).toBeNull();
    expect(container.querySelector("span")).not.toBeNull();
  });

  it("renders constant series as a centred flat line (no div-by-zero)", () => {
    const { container } = render(<Sparkline values={[5, 5, 5, 5]} height={20} />);
    const pts = container.querySelector("polyline")!.getAttribute("points") ?? "";
    for (const tok of pts.split(" ")) {
      const [, y] = tok.split(",");
      expect(Number(y)).toBe(10); // height / 2
    }
  });

  it("respects ariaLabel for screen readers", () => {
    const { container } = render(
      <Sparkline values={[1, 2, 3]} ariaLabel="run trend" />,
    );
    const svg = container.querySelector("svg");
    expect(svg!.getAttribute("role")).toBe("img");
    expect(svg!.getAttribute("aria-label")).toBe("run trend");
  });
});
