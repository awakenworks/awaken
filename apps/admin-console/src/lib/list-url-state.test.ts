import { describe, expect, it } from "vitest";
import {
  readListState,
  writeListState,
  readSkillsFilter,
  writeSkillsFilter,
  readFixtureFilter,
  writeFixtureFilter,
  type ListState,
} from "./list-url-state";
import { DEFAULT_PAGE_SIZE } from "./list-view";
import { DEFAULT_SKILLS_FILTER } from "./skills-filter";
import { DEFAULT_FIXTURE_FILTER } from "./eval-reports-filter";

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

type AgentKey = "id" | "model_id" | "plugin_count";

const AGENT_OPTIONS = {
  validSortKeys: ["id", "model_id", "plugin_count"] as const,
  defaultSort: { key: "id" as AgentKey, direction: "asc" as const },
  defaultPageSize: DEFAULT_PAGE_SIZE,
} satisfies Parameters<typeof readListState<AgentKey>>[1];

function p(search: string) {
  return new URLSearchParams(search);
}

// ---------------------------------------------------------------------------
// readListState
// ---------------------------------------------------------------------------

describe("readListState", () => {
  it("returns defaults when URL is empty", () => {
    const state = readListState(p(""), AGENT_OPTIONS);
    expect(state.search).toBe("");
    expect(state.sort).toEqual({ key: "id", direction: "asc" });
    expect(state.pageSize).toBe(DEFAULT_PAGE_SIZE);
    expect(state.page).toBe(1);
  });

  it("parses valid params correctly", () => {
    const state = readListState(
      p("q=hello&sort=model_id&dir=desc&size=50&page=3"),
      AGENT_OPTIONS,
    );
    expect(state.search).toBe("hello");
    expect(state.sort).toEqual({ key: "model_id", direction: "desc" });
    expect(state.pageSize).toBe(50);
    expect(state.page).toBe(3);
  });

  it("ignores unknown sort key and uses default sort", () => {
    const state = readListState(p("sort=nonexistent&dir=asc"), AGENT_OPTIONS);
    expect(state.sort).toEqual({ key: "id", direction: "asc" });
  });

  it("clamps invalid page size to default", () => {
    const state = readListState(p("size=999"), AGENT_OPTIONS);
    expect(state.pageSize).toBe(DEFAULT_PAGE_SIZE);
  });

  it("clamps page to 1 when value is 0", () => {
    const state = readListState(p("page=0"), AGENT_OPTIONS);
    expect(state.page).toBe(1);
  });

  it("clamps page to 1 when value is negative", () => {
    const state = readListState(p("page=-5"), AGENT_OPTIONS);
    expect(state.page).toBe(1);
  });

  it("clamps page to 1 when value is non-numeric", () => {
    const state = readListState(p("page=abc"), AGENT_OPTIONS);
    expect(state.page).toBe(1);
  });

  it("defaults dir to asc when dir is absent but sort key is present", () => {
    const state = readListState(p("sort=plugin_count"), AGENT_OPTIONS);
    expect(state.sort).toEqual({ key: "plugin_count", direction: "asc" });
  });

  it("defaults dir to asc when dir is invalid", () => {
    const state = readListState(p("sort=model_id&dir=sideways"), AGENT_OPTIONS);
    expect(state.sort).toEqual({ key: "model_id", direction: "asc" });
  });
});

// ---------------------------------------------------------------------------
// writeListState
// ---------------------------------------------------------------------------

describe("writeListState", () => {
  it("omits all params when state equals defaults", () => {
    const state: ListState<AgentKey> = {
      search: "",
      sort: { key: "id", direction: "asc" },
      pageSize: DEFAULT_PAGE_SIZE,
      page: 1,
    };
    const result = writeListState(p(""), state, AGENT_OPTIONS);
    expect(result.toString()).toBe("");
  });

  it("includes q when search is non-empty", () => {
    const state: ListState<AgentKey> = {
      search: "foo",
      sort: { key: "id", direction: "asc" },
      pageSize: DEFAULT_PAGE_SIZE,
      page: 1,
    };
    const result = writeListState(p(""), state, AGENT_OPTIONS);
    expect(result.get("q")).toBe("foo");
  });

  it("includes sort+dir=desc when sort differs from default with desc direction", () => {
    const state: ListState<AgentKey> = {
      search: "",
      sort: { key: "model_id", direction: "desc" },
      pageSize: DEFAULT_PAGE_SIZE,
      page: 1,
    };
    const result = writeListState(p(""), state, AGENT_OPTIONS);
    expect(result.get("sort")).toBe("model_id");
    expect(result.get("dir")).toBe("desc");
  });

  it("omits dir when sort differs from default but direction is asc", () => {
    const state: ListState<AgentKey> = {
      search: "",
      sort: { key: "model_id", direction: "asc" },
      pageSize: DEFAULT_PAGE_SIZE,
      page: 1,
    };
    const result = writeListState(p(""), state, AGENT_OPTIONS);
    expect(result.get("sort")).toBe("model_id");
    expect(result.get("dir")).toBeNull();
  });

  it("includes size when pageSize differs from default", () => {
    const state: ListState<AgentKey> = {
      search: "",
      sort: { key: "id", direction: "asc" },
      pageSize: 50,
      page: 1,
    };
    const result = writeListState(p(""), state, AGENT_OPTIONS);
    expect(result.get("size")).toBe("50");
  });

  it("includes page when page > 1", () => {
    const state: ListState<AgentKey> = {
      search: "",
      sort: { key: "id", direction: "asc" },
      pageSize: DEFAULT_PAGE_SIZE,
      page: 5,
    };
    const result = writeListState(p(""), state, AGENT_OPTIONS);
    expect(result.get("page")).toBe("5");
  });

  it("omits sort+dir when sort is null and default is null", () => {
    const optionsNoDefault = {
      validSortKeys: ["id", "model_id"] as const,
      defaultSort: null,
    };
    const state: ListState<"id" | "model_id"> = {
      search: "",
      sort: null,
      pageSize: DEFAULT_PAGE_SIZE,
      page: 1,
    };
    const result = writeListState(p(""), state, optionsNoDefault);
    expect(result.get("sort")).toBeNull();
    expect(result.get("dir")).toBeNull();
  });

  it("preserves unrelated params already in the search string", () => {
    const state: ListState<AgentKey> = {
      search: "bar",
      sort: { key: "id", direction: "asc" },
      pageSize: DEFAULT_PAGE_SIZE,
      page: 1,
    };
    const result = writeListState(p("tab=tools"), state, AGENT_OPTIONS);
    expect(result.get("tab")).toBe("tools");
    expect(result.get("q")).toBe("bar");
  });
});

// ---------------------------------------------------------------------------
// Round-trip
// ---------------------------------------------------------------------------

describe("round-trip readListState / writeListState", () => {
  it("round-trips a fully non-default state", () => {
    const original: ListState<AgentKey> = {
      search: "alpha",
      sort: { key: "plugin_count", direction: "desc" },
      pageSize: 100,
      page: 7,
    };
    const written = writeListState(p(""), original, AGENT_OPTIONS);
    const recovered = readListState(written, AGENT_OPTIONS);
    expect(recovered).toEqual(original);
  });

  it("round-trips the default state without adding params", () => {
    const original: ListState<AgentKey> = {
      search: "",
      sort: { key: "id", direction: "asc" },
      pageSize: DEFAULT_PAGE_SIZE,
      page: 1,
    };
    const written = writeListState(p(""), original, AGENT_OPTIONS);
    expect(written.toString()).toBe("");
    const recovered = readListState(written, AGENT_OPTIONS);
    expect(recovered).toEqual(original);
  });
});

// ---------------------------------------------------------------------------
// readSkillsFilter / writeSkillsFilter
// ---------------------------------------------------------------------------

describe("readSkillsFilter", () => {
  it("returns defaults when URL is empty", () => {
    expect(readSkillsFilter(p(""))).toEqual(DEFAULT_SKILLS_FILTER);
  });

  it("parses valid caller and ctx params", () => {
    const state = readSkillsFilter(p("q=tool&caller=user&ctx=fork"));
    expect(state).toEqual({ search: "tool", invocable: "user", context: "fork" });
  });

  it("ignores invalid caller and falls back to default", () => {
    const state = readSkillsFilter(p("caller=bogus"));
    expect(state.invocable).toBe(DEFAULT_SKILLS_FILTER.invocable);
  });

  it("ignores invalid ctx and falls back to default", () => {
    const state = readSkillsFilter(p("ctx=bogus"));
    expect(state.context).toBe(DEFAULT_SKILLS_FILTER.context);
  });
});

describe("writeSkillsFilter", () => {
  it("omits all params when state equals defaults", () => {
    const result = writeSkillsFilter(p(""), DEFAULT_SKILLS_FILTER);
    expect(result.toString()).toBe("");
  });

  it("includes non-default caller and ctx", () => {
    const result = writeSkillsFilter(p(""), {
      search: "",
      invocable: "model",
      context: "inline",
    });
    expect(result.get("caller")).toBe("model");
    expect(result.get("ctx")).toBe("inline");
    expect(result.get("q")).toBeNull();
  });

  it("round-trips non-default skills filter", () => {
    const original = { search: "playwright", invocable: "user" as const, context: "fork" as const };
    const written = writeSkillsFilter(p(""), original);
    const recovered = readSkillsFilter(written);
    expect(recovered).toEqual(original);
  });
});

// ---------------------------------------------------------------------------
// readFixtureFilter / writeFixtureFilter
// ---------------------------------------------------------------------------

describe("readFixtureFilter", () => {
  it("returns defaults when URL is empty", () => {
    expect(readFixtureFilter(p(""))).toEqual(DEFAULT_FIXTURE_FILTER);
  });

  it("parses valid status and search params", () => {
    const state = readFixtureFilter(p("q=fixture1&status=failed"));
    expect(state).toEqual({ search: "fixture1", status: "failed" });
  });

  it("ignores invalid status and falls back to default", () => {
    const state = readFixtureFilter(p("status=unknown"));
    expect(state.status).toBe(DEFAULT_FIXTURE_FILTER.status);
  });
});

describe("writeFixtureFilter", () => {
  it("omits all params when state equals defaults", () => {
    const result = writeFixtureFilter(p(""), DEFAULT_FIXTURE_FILTER);
    expect(result.toString()).toBe("");
  });

  it("includes non-default status", () => {
    const result = writeFixtureFilter(p(""), { search: "", status: "regressions" });
    expect(result.get("status")).toBe("regressions");
  });

  it("round-trips non-default fixture filter", () => {
    const original = { search: "perf-test", status: "passed" as const };
    const written = writeFixtureFilter(p(""), original);
    const recovered = readFixtureFilter(written);
    expect(recovered).toEqual(original);
  });
});
