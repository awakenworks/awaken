import { describe, expect, it } from "vitest";
import {
  DEFAULT_SKILLS_FILTER,
  filterSkills,
  type SkillsFilterState,
} from "./skills-filter";
import type { SkillInfo } from "./config-api";

function makeSkill(overrides: Partial<SkillInfo> = {}): SkillInfo {
  return {
    id: "greeting",
    name: "Greeting",
    description: "Friendly opener",
    allowed_tools: [],
    when_to_use: null,
    arguments: [],
    argument_hint: null,
    user_invocable: true,
    model_invocable: false,
    model_override: null,
    context: "inline",
    paths: [],
    ...overrides,
  };
}

const SKILLS: SkillInfo[] = [
  makeSkill({
    id: "greeting",
    name: "Greeting",
    description: "Friendly opener",
    user_invocable: true,
    model_invocable: false,
    context: "inline",
    allowed_tools: ["Read"],
  }),
  makeSkill({
    id: "researcher",
    name: "Researcher",
    description: "Investigates topics",
    user_invocable: false,
    model_invocable: true,
    context: "fork",
    paths: ["docs/", "src/"],
  }),
  makeSkill({
    id: "internal-helper",
    name: "Internal Helper",
    description: "Background utility",
    user_invocable: false,
    model_invocable: false,
    context: "inline",
  }),
];

function withFilter(overrides: Partial<SkillsFilterState>): SkillsFilterState {
  return { ...DEFAULT_SKILLS_FILTER, ...overrides };
}

describe("filterSkills", () => {
  it("returns every skill with the default filter", () => {
    expect(filterSkills(SKILLS, DEFAULT_SKILLS_FILTER)).toEqual(SKILLS);
  });

  it("filters down to user-invocable skills", () => {
    expect(
      filterSkills(SKILLS, withFilter({ invocable: "user" })).map((s) => s.id),
    ).toEqual(["greeting"]);
  });

  it("filters down to model-invocable skills", () => {
    expect(
      filterSkills(SKILLS, withFilter({ invocable: "model" })).map((s) => s.id),
    ).toEqual(["researcher"]);
  });

  it("filters down to internal-only skills", () => {
    expect(
      filterSkills(SKILLS, withFilter({ invocable: "internal" })).map(
        (s) => s.id,
      ),
    ).toEqual(["internal-helper"]);
  });

  it("filters by execution context", () => {
    expect(
      filterSkills(SKILLS, withFilter({ context: "fork" })).map((s) => s.id),
    ).toEqual(["researcher"]);
  });

  it("matches search across id, name, description, when_to_use, tools, paths", () => {
    expect(
      filterSkills(SKILLS, withFilter({ search: "investigates" })).map(
        (s) => s.id,
      ),
    ).toEqual(["researcher"]);
    expect(
      filterSkills(SKILLS, withFilter({ search: "Read" })).map((s) => s.id),
    ).toEqual(["greeting"]);
    expect(
      filterSkills(SKILLS, withFilter({ search: "docs" })).map((s) => s.id),
    ).toEqual(["researcher"]);
  });

  it("requires every search token to match", () => {
    expect(
      filterSkills(SKILLS, withFilter({ search: "researcher topics" })).map(
        (s) => s.id,
      ),
    ).toEqual(["researcher"]);
    expect(
      filterSkills(SKILLS, withFilter({ search: "researcher zzz" })),
    ).toEqual([]);
  });

  it("composes multiple filters", () => {
    expect(
      filterSkills(
        SKILLS,
        withFilter({ invocable: "model", context: "fork" }),
      ).map((s) => s.id),
    ).toEqual(["researcher"]);
    expect(
      filterSkills(
        SKILLS,
        withFilter({ invocable: "user", context: "fork" }),
      ),
    ).toEqual([]);
  });
});
