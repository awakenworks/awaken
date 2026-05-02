// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { Eyebrow } from "./eyebrow";
import { Pill, PillStack } from "./pill";
import { SkeletonRows, SkeletonBlock } from "./skeleton";
import { EmptyState } from "./empty-state";
import { PageHeader } from "./page-header";
import { FilterBar, FilterChip } from "./filter-bar";

afterEach(() => cleanup());

describe("Eyebrow", () => {
  it("renders uppercase tracked text", () => {
    render(<Eyebrow>Configure</Eyebrow>);
    const el = screen.getByText("Configure");
    expect(el.className).toMatch(/uppercase/);
    expect(el.className).toMatch(/tracking-/);
  });
});

describe("Pill", () => {
  it("applies tone class", () => {
    render(<Pill tone="warn">draft</Pill>);
    const el = screen.getByText("draft");
    expect(el.className).toMatch(/tone-warn/);
  });

  it("defaults to neutral tone", () => {
    render(<Pill>x</Pill>);
    expect(screen.getByText("x").className).toMatch(/text-fg-soft/);
  });
});

describe("PillStack", () => {
  it("shows up to max items and a +N overflow pill", () => {
    render(<PillStack items={["a", "b", "c", "d", "e"]} max={3} />);
    expect(screen.getByText("a")).toBeDefined();
    expect(screen.getByText("b")).toBeDefined();
    expect(screen.getByText("c")).toBeDefined();
    expect(screen.queryByText("d")).toBeNull();
    expect(screen.getByText("+2")).toBeDefined();
  });

  it("does not render an overflow pill when count <= max", () => {
    render(<PillStack items={["a", "b"]} max={3} />);
    expect(screen.queryByText(/^\+/)).toBeNull();
  });

  it("renders empty placeholder when no items", () => {
    render(<PillStack items={[]} empty="None" />);
    expect(screen.getByText("None")).toBeDefined();
  });
});

describe("SkeletonRows", () => {
  it("renders the requested row × col grid", () => {
    const { container } = render(
      <table>
        <tbody>
          <SkeletonRows rows={2} cols={4} />
        </tbody>
      </table>,
    );
    expect(container.querySelectorAll("tr").length).toBe(2);
    expect(container.querySelectorAll("td").length).toBe(8);
  });

  it("each cell holds an aria-hidden shimmer block", () => {
    const { container } = render(
      <table>
        <tbody>
          <SkeletonRows rows={1} cols={2} />
        </tbody>
      </table>,
    );
    const blocks = container.querySelectorAll('[aria-hidden="true"]');
    expect(blocks.length).toBe(2);
  });
});

describe("SkeletonBlock", () => {
  it("respects width/height props", () => {
    render(<SkeletonBlock width="42px" height="6px" />);
    const el = document.querySelector('[aria-hidden="true"]') as HTMLElement;
    expect(el.style.width).toBe("42px");
    expect(el.style.height).toBe("6px");
  });
});

describe("EmptyState", () => {
  it("renders title + description + actions", () => {
    render(
      <EmptyState
        title="Nothing yet"
        description="Make one to get started."
        actions={<button>Create</button>}
      />,
    );
    expect(screen.getByText("Nothing yet")).toBeDefined();
    expect(screen.getByText("Make one to get started.")).toBeDefined();
    expect(screen.getByRole("button", { name: "Create" })).toBeDefined();
  });
});

describe("PageHeader", () => {
  it("shows eyebrow + title + count + actions", () => {
    render(
      <PageHeader
        eyebrow="Configure"
        title="Agents"
        count={14}
        actions={<button>+ New</button>}
      />,
    );
    expect(screen.getByText("Configure")).toBeDefined();
    // Heading accessible name is exactly the title — count is aria-hidden
    // so it does not pollute screen reader output (and tests can find it).
    expect(screen.getByRole("heading", { name: "Agents" })).toBeDefined();
    expect(screen.getByText("14")).toBeDefined();
    expect(screen.getByRole("button", { name: "+ New" })).toBeDefined();
  });
});

describe("FilterBar", () => {
  it("renders Filter / Sort / meta sections only when provided", () => {
    render(<FilterBar meta="14 of 14" />);
    expect(screen.getByText("14 of 14")).toBeDefined();
    expect(screen.queryByText("Filter")).toBeNull();
    expect(screen.queryByText("Sort")).toBeNull();
  });
});

describe("FilterChip", () => {
  it("emits onChange when the user picks a new option", () => {
    const handle = vi.fn();
    render(
      <FilterChip
        label="model"
        value="any"
        options={[
          { value: "any", label: "any" },
          { value: "gpt-4o", label: "gpt-4o" },
        ]}
        onChange={handle}
      />,
    );
    const select = screen.getByRole("combobox", { name: /model/i });
    fireEvent.change(select, { target: { value: "gpt-4o" } });
    expect(handle).toHaveBeenCalledWith("gpt-4o");
  });
});
