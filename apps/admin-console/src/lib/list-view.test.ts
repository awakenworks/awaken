import { describe, expect, it } from "vitest";
import {
  compareBoolean,
  compareNumber,
  compareString,
  DEFAULT_PAGE_SIZE,
  filterBySearch,
  PAGE_SIZE_OPTIONS,
  paginate,
  sortItems,
  toggleSort,
  type SortConfig,
  type SortState,
} from "./list-view";

interface Row {
  id: string;
  count: number;
  active: boolean;
}

const ROWS: Row[] = [
  { id: "alpha", count: 3, active: true },
  { id: "Beta", count: 1, active: false },
  { id: "gamma", count: 2, active: true },
  { id: "alpha-prime", count: 0, active: false },
];

const SORT_CONFIG: SortConfig<Row, "id" | "count" | "active"> = {
  id: (a, b) => compareString(a.id, b.id),
  count: (a, b) => compareNumber(a.count, b.count),
  active: (a, b) => compareBoolean(a.active, b.active),
};

describe("filterBySearch", () => {
  it("returns items unchanged when the query is empty or whitespace", () => {
    expect(filterBySearch(ROWS, "", (row) => [row.id])).toBe(ROWS);
    expect(filterBySearch(ROWS, "   ", (row) => [row.id])).toBe(ROWS);
  });

  it("matches case-insensitively across all selector outputs", () => {
    const matched = filterBySearch(ROWS, "BETA", (row) => [row.id]);
    expect(matched.map((row) => row.id)).toEqual(["Beta"]);
  });

  it("requires every whitespace-delimited token to match somewhere", () => {
    const matched = filterBySearch(ROWS, "alpha prime", (row) => [row.id]);
    expect(matched.map((row) => row.id)).toEqual(["alpha-prime"]);
  });

  it("ignores nullish selector outputs", () => {
    const items = [
      { id: "x", description: undefined as string | undefined },
      { id: "y", description: "hidden gem" },
    ];
    const matched = filterBySearch(items, "gem", (row) => [
      row.id,
      row.description,
    ]);
    expect(matched.map((r) => r.id)).toEqual(["y"]);
  });
});

describe("sortItems", () => {
  it("returns the original array when no sort is set", () => {
    expect(sortItems(ROWS, null, SORT_CONFIG)).toBe(ROWS);
  });

  it("sorts ascending by string", () => {
    const state: SortState<"id"> = { key: "id", direction: "asc" };
    expect(sortItems(ROWS, state, SORT_CONFIG).map((r) => r.id)).toEqual([
      "alpha",
      "alpha-prime",
      "Beta",
      "gamma",
    ]);
  });

  it("sorts descending by number", () => {
    const state: SortState<"count"> = { key: "count", direction: "desc" };
    expect(sortItems(ROWS, state, SORT_CONFIG).map((r) => r.count)).toEqual([
      3, 2, 1, 0,
    ]);
  });

  it("sorts booleans with false first when ascending", () => {
    const state: SortState<"active"> = { key: "active", direction: "asc" };
    const sorted = sortItems(ROWS, state, SORT_CONFIG);
    expect(sorted.slice(0, 2).every((r) => !r.active)).toBe(true);
    expect(sorted.slice(2).every((r) => r.active)).toBe(true);
  });

  it("ignores unknown sort keys", () => {
    const state = { key: "missing" as never, direction: "asc" } as SortState<never>;
    expect(sortItems(ROWS, state, SORT_CONFIG as never)).toBe(ROWS);
  });
});

describe("toggleSort", () => {
  it("starts a new sort ascending", () => {
    expect(toggleSort(null, "id")).toEqual({ key: "id", direction: "asc" });
  });

  it("flips direction on the same key", () => {
    expect(toggleSort({ key: "id", direction: "asc" }, "id")).toEqual({
      key: "id",
      direction: "desc",
    });
  });

  it("clears the sort on the third toggle of the same key", () => {
    expect(toggleSort({ key: "id", direction: "desc" }, "id")).toBeNull();
  });

  it("switches to a new key in ascending mode", () => {
    expect(toggleSort({ key: "id", direction: "desc" }, "count")).toEqual({
      key: "count",
      direction: "asc",
    });
  });
});

describe("paginate", () => {
  const items = Array.from({ length: 23 }, (_, idx) => ({ id: `i${idx}` }));

  it("returns the requested page slice", () => {
    const view = paginate(items, { page: 2, pageSize: 10, totalItems: items.length });
    expect(view.items).toHaveLength(10);
    expect(view.items[0].id).toBe("i10");
    expect(view.startIndex).toBe(10);
    expect(view.endIndex).toBe(20);
    expect(view.pageCount).toBe(3);
  });

  it("clamps the page to the valid range", () => {
    const view = paginate(items, { page: 99, pageSize: 10, totalItems: items.length });
    expect(view.page).toBe(3);
    expect(view.items[0].id).toBe("i20");
    expect(view.endIndex).toBe(23);
  });

  it("treats an empty list as a single-page view", () => {
    const view = paginate([], { page: 1, pageSize: 10, totalItems: 0 });
    expect(view.pageCount).toBe(1);
    expect(view.items).toEqual([]);
  });

  it("respects the chosen page size", () => {
    const view = paginate(items, { page: 1, pageSize: 50, totalItems: items.length });
    expect(view.items).toHaveLength(items.length);
    expect(view.pageCount).toBe(1);
  });
});

describe("constants", () => {
  it("exposes sane defaults", () => {
    expect(PAGE_SIZE_OPTIONS).toContain(DEFAULT_PAGE_SIZE);
  });
});
