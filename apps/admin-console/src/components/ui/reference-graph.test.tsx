// @vitest-environment jsdom
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render } from "@testing-library/react";
import { ReferenceGraph } from "./reference-graph";

afterEach(() => cleanup());

describe("ReferenceGraph", () => {
  it("renders empty state when columns is empty", () => {
    const { container } = render(<ReferenceGraph columns={[]} edges={[]} />);
    expect(container.textContent).toMatch(/No nodes to graph yet/);
  });

  it("renders all node labels and column headers", () => {
    const { container } = render(
      <ReferenceGraph
        columns={[
          {
            id: "a",
            label: "Agents",
            nodes: [
              { id: "agent:foo", label: "foo", sub: "model-x" },
              { id: "agent:bar", label: "bar", sub: "model-y" },
            ],
          },
          {
            id: "m",
            label: "Models",
            nodes: [{ id: "model:model-x", label: "model-x" }],
          },
        ]}
        edges={[{ from: "agent:foo", to: "model:model-x" }]}
      />,
    );
    expect(container.textContent).toMatch(/Agents/);
    expect(container.textContent).toMatch(/Models/);
    expect(container.textContent).toMatch(/foo/);
    expect(container.textContent).toMatch(/bar/);
    expect(container.textContent).toMatch(/model-x/);
  });

  it("draws an SVG path per resolvable edge, skips dangling refs", () => {
    const { container } = render(
      <ReferenceGraph
        columns={[
          { id: "a", label: "A", nodes: [{ id: "n1", label: "n1" }] },
          { id: "b", label: "B", nodes: [{ id: "n2", label: "n2" }] },
        ]}
        edges={[
          { from: "n1", to: "n2" },
          { from: "n1", to: "missing" },
          { from: "ghost", to: "n2" },
        ]}
      />,
    );
    const paths = container.querySelectorAll("svg path");
    expect(paths.length).toBe(1);
  });
});
