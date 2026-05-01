import type { SkillInfo } from "./config-api";

export type InvocableFilter = "any" | "user" | "model" | "internal";
export type ContextFilter = "any" | "inline" | "fork";

export interface SkillsFilterState {
  search: string;
  invocable: InvocableFilter;
  context: ContextFilter;
}

export const DEFAULT_SKILLS_FILTER: SkillsFilterState = {
  search: "",
  invocable: "any",
  context: "any",
};

/// Apply the user-controllable filters to the skills list.
export function filterSkills(
  skills: SkillInfo[],
  filter: SkillsFilterState,
): SkillInfo[] {
  return skills.filter((skill) => {
    if (!matchesInvocable(skill, filter.invocable)) return false;
    if (!matchesContext(skill, filter.context)) return false;
    if (!matchesSearch(skill, filter.search)) return false;
    return true;
  });
}

function matchesInvocable(skill: SkillInfo, filter: InvocableFilter): boolean {
  switch (filter) {
    case "any":
      return true;
    case "user":
      return skill.user_invocable;
    case "model":
      return skill.model_invocable;
    case "internal":
      return !skill.user_invocable && !skill.model_invocable;
  }
}

function matchesContext(skill: SkillInfo, filter: ContextFilter): boolean {
  if (filter === "any") return true;
  return skill.context === filter;
}

function matchesSearch(skill: SkillInfo, query: string): boolean {
  const trimmed = query.trim().toLowerCase();
  if (trimmed.length === 0) return true;
  const tokens = trimmed.split(/\s+/).filter((t) => t.length > 0);
  if (tokens.length === 0) return true;
  const haystack = [
    skill.id,
    skill.name,
    skill.description,
    skill.when_to_use ?? "",
    ...skill.allowed_tools,
    ...skill.paths,
  ]
    .join("  ")
    .toLowerCase();
  return tokens.every((token) => haystack.includes(token));
}
