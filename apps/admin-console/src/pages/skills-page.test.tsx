// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router";
import { SkillsPage } from "./skills-page";
import type { SkillInfo } from "@/lib/api";

const queryState = vi.hoisted(() => ({
  value: {
    data: undefined as { skills: SkillInfo[] } | undefined,
    isPending: false,
    error: null as unknown,
  },
  toastError: vi.fn(),
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("@/lib/query/hooks/capabilities", () => ({
  useCapabilitiesQuery: () => queryState.value,
}));

vi.mock("@/components/toast-provider", () => ({
  useToast: () => ({ error: queryState.toastError }),
}));

function skill(overrides: Partial<SkillInfo>): SkillInfo {
  return {
    id: "imagegen",
    name: "Image Generator",
    description: "Generate raster images from prompts",
    allowed_tools: ["image_gen"],
    when_to_use: "Use when a bitmap asset is needed",
    arguments: [{ name: "prompt", description: "Image prompt", required: true }],
    user_invocable: true,
    model_invocable: false,
    context: "inline",
    paths: ["assets/**"],
    ...overrides,
  };
}

function renderSkills() {
  return render(
    <MemoryRouter initialEntries={["/skills"]}>
      <SkillsPage />
    </MemoryRouter>,
  );
}

beforeEach(() => {
  queryState.value = { data: undefined, isPending: false, error: null };
  queryState.toastError.mockReset();
});

afterEach(() => {
  cleanup();
});

describe("SkillsPage", () => {
  it("renders skill cards with activation hints, tools, paths, arguments, and injection preview", () => {
    queryState.value = {
      isPending: false,
      error: null,
      data: {
        skills: [
          skill({ id: "imagegen", name: "Image Generator" }),
          skill({
            id: "planner",
            name: "planner",
            description: "Internal planning helper",
            allowed_tools: [],
            when_to_use: null,
            arguments: [],
            user_invocable: false,
            model_invocable: false,
            context: "fork",
            paths: [],
          }),
        ],
      },
    };

    renderSkills();

    expect(screen.getByText("Image Generator")).toBeTruthy();
    expect(screen.getByText("Generate raster images from prompts")).toBeTruthy();
    expect(screen.getByText("Use when a bitmap asset is needed")).toBeTruthy();
    expect(screen.getByText("image_gen")).toBeTruthy();
    expect(screen.getByText("assets/**")).toBeTruthy();
    expect(screen.getByText("prompt")).toBeTruthy();
    expect(screen.getByText("required")).toBeTruthy();
    expect(screen.getByText(/# Skill: Image Generator/)).toBeTruthy();
    expect(screen.getByText("2 of 2 shown")).toBeTruthy();

    expect(screen.getByText("planner")).toBeTruthy();
    expect(screen.getByText("No explicit tool filter.")).toBeTruthy();
    expect(screen.getByText("Unscoped (any path).")).toBeTruthy();
    expect(screen.getByText("No formal arguments declared.")).toBeTruthy();
  });

  it("filters by search, caller type, and context using URL-backed controls", () => {
    queryState.value = {
      isPending: false,
      error: null,
      data: {
        skills: [
          skill({ id: "imagegen", name: "Image Generator" }),
          skill({
            id: "code-review",
            name: "Code Review",
            description: "Review patches and tests",
            allowed_tools: ["Read", "Grep"],
            user_invocable: false,
            model_invocable: true,
            context: "inline",
          }),
          skill({
            id: "internal-plan",
            name: "Internal Planner",
            description: "Private fork planning",
            allowed_tools: [],
            arguments: [],
            user_invocable: false,
            model_invocable: false,
            context: "fork",
          }),
        ],
      },
    };

    renderSkills();

    fireEvent.change(screen.getByPlaceholderText(/Search by id/), {
      target: { value: "review" },
    });
    expect(screen.getByText("1 of 3 shown")).toBeTruthy();
    expect(screen.getByText("Code Review")).toBeTruthy();
    expect(screen.queryByText("Image Generator")).toBeNull();

    fireEvent.change(screen.getByLabelText("Caller"), {
      target: { value: "internal" },
    });
    expect(screen.getByText("0 of 3 shown")).toBeTruthy();
    expect(screen.getByText("skills.noMatches.title")).toBeTruthy();

    fireEvent.change(screen.getByPlaceholderText(/Search by id/), {
      target: { value: "" },
    });
    fireEvent.change(screen.getByLabelText("Context"), {
      target: { value: "fork" },
    });
    expect(screen.getByText("1 of 3 shown")).toBeTruthy();
    expect(screen.getByText("Internal Planner")).toBeTruthy();
  });

  it("shows empty and error states with exact user-facing signals", () => {
    queryState.value = {
      isPending: false,
      error: new Error("capabilities unavailable"),
      data: { skills: [] },
    };

    renderSkills();

    expect(screen.getByText("skills.empty.title")).toBeTruthy();
    expect(screen.getByText("skills.empty.desc")).toBeTruthy();
    expect(queryState.toastError).toHaveBeenCalledWith("capabilities unavailable");
  });
});
