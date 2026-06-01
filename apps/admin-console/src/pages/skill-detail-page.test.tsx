// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";
import { SkillDetailPage } from "./skill-detail-page";
import type { AgentSpec, Capabilities, CapabilitiesResult, SkillInfo } from "@/lib/api";

const queryState = vi.hoisted(() => ({
  capabilities: {
    data: undefined as CapabilitiesResult | undefined,
    isPending: false,
    error: null as unknown,
  },
  agents: {
    data: undefined as { items: AgentSpec[] } | undefined,
    isPending: false,
    error: null as unknown,
  },
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

vi.mock("@/lib/query/hooks/capabilities", () => ({
  useCapabilitiesQuery: () => queryState.capabilities,
}));

vi.mock("@/lib/query/hooks/config", () => ({
  useConfigListQuery: () => queryState.agents,
}));

function skill(overrides: Partial<SkillInfo> = {}): SkillInfo {
  return {
    id: "imagegen",
    name: "Image Generator",
    description: "Generate raster images from prompts",
    allowed_tools: ["image_gen"],
    when_to_use: "Use when a bitmap asset is needed",
    arguments: [
      { name: "prompt", description: "Image prompt", required: true },
      { name: "size", description: null, required: false },
    ],
    user_invocable: true,
    model_invocable: true,
    context: "inline",
    paths: ["assets/**"],
    ...overrides,
  };
}

function agent(overrides: Partial<AgentSpec>): AgentSpec {
  return {
    id: "agent-a",
    model_id: "model-a",
    system_prompt: "help",
    ...overrides,
  };
}

function capabilities(skills: SkillInfo[]): CapabilitiesResult {
  return {
    kind: "ok",
    capabilities: {
      agents: [],
      tools: [],
      plugins: [],
      skills,
      models: [],
      providers: [],
      namespaces: [],
    } satisfies Capabilities,
  };
}

function renderDetail(path: string, route = "/skills/:id") {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route path={route} element={<SkillDetailPage />} />
      </Routes>
    </MemoryRouter>,
  );
}

beforeEach(() => {
  queryState.capabilities = { data: undefined, isPending: false, error: null };
  queryState.agents = { data: undefined, isPending: false, error: null };
});

afterEach(() => {
  cleanup();
});

describe("SkillDetailPage", () => {
  it("renders a missing-id state when no route param is present", () => {
    renderDetail("/skills", "/skills");

    expect(screen.getByText("Missing skill id.")).toBeTruthy();
  });

  it("shows loading, error, and not-found states from the capabilities query", () => {
    const { unmount } = renderDetail("/skills/imagegen");
    expect(screen.getByText("common.loading")).toBeTruthy();
    unmount();

    queryState.capabilities = {
      data: undefined,
      isPending: false,
      error: new Error("capabilities unavailable"),
    };
    const errorView = renderDetail("/skills/imagegen");
    expect(screen.getByText("capabilities unavailable")).toBeTruthy();
    errorView.unmount();

    queryState.capabilities = { data: capabilities([]), isPending: false, error: null };
    renderDetail("/skills/imagegen");
    expect(screen.getByText("trace.notFound")).toBeTruthy();
  });

  it("renders details, injection preview, and all supported agent reference shapes", () => {
    queryState.capabilities = {
      data: capabilities([skill()]),
      isPending: false,
      error: null,
    };
    queryState.agents = {
      data: {
        items: [
          agent({ id: "plugin-agent", model_id: "claude", plugin_ids: ["imagegen"] }),
          agent({
            id: "allowlist-agent",
            model_id: "gpt",
            sections: { skills: { allowlist: ["imagegen"] } },
          }),
          agent({
            id: "ids-agent",
            model_id: "local",
            sections: { "skills-discovery": { ids: ["imagegen"] } },
          }),
          agent({ id: "other-agent", model_id: "other", plugin_ids: ["other"] }),
        ],
      },
      isPending: false,
      error: null,
    };

    renderDetail("/skills/imagegen");

    expect(screen.getByRole("heading", { name: "Image Generator" })).toBeTruthy();
    expect(screen.getByText("imagegen")).toBeTruthy();
    expect(screen.getByText("Generate raster images from prompts")).toBeTruthy();
    expect(screen.getByText("Use when a bitmap asset is needed")).toBeTruthy();
    expect(screen.getByText("image_gen")).toBeTruthy();
    expect(screen.getByText("assets/**")).toBeTruthy();
    expect(screen.getByText("prompt")).toBeTruthy();
    expect(screen.getByText("Image prompt")).toBeTruthy();
    expect(screen.getByText("size")).toBeTruthy();
    expect(screen.getByText("skills.required")).toBeTruthy();
    expect(screen.getByText("skills.optional")).toBeTruthy();
    expect(screen.getByRole("link", { name: "plugin-agent" }).getAttribute("href")).toBe(
      "/agents/plugin-agent",
    );
    expect(screen.getByRole("link", { name: "allowlist-agent" }).getAttribute("href")).toBe(
      "/agents/allowlist-agent",
    );
    expect(screen.getByRole("link", { name: "ids-agent" }).getAttribute("href")).toBe(
      "/agents/ids-agent",
    );
    expect(screen.queryByText("other-agent")).toBeNull();
    const preview = screen.getByText(/# Skill: Image Generator/);
    expect(preview.textContent).toContain("Arguments:\n  - prompt (required): Image prompt");
  });

  it("renders empty optional sections without inventing tool, path, argument, or agent data", () => {
    queryState.capabilities = {
      data: capabilities([
        skill({
          id: "planner",
          name: "planner",
          description: "Plan work",
          allowed_tools: [],
          when_to_use: null,
          arguments: [],
          user_invocable: false,
          model_invocable: false,
          context: "fork",
          paths: [],
        }),
      ]),
      isPending: false,
      error: null,
    };
    queryState.agents = { data: { items: [] }, isPending: false, error: null };

    renderDetail("/skills/planner");

    expect(screen.getByRole("heading", { name: "planner" })).toBeTruthy();
    expect(screen.queryByText("When to use")).toBeNull();
    expect(screen.getByText("skills.noToolFilter")).toBeTruthy();
    expect(screen.getByText("skills.unscopedPath")).toBeTruthy();
    expect(screen.getByText("skills.noArguments")).toBeTruthy();
    expect(screen.getByText("No agents reference this skill.")).toBeTruthy();
    expect(screen.queryByText("skills.user")).toBeNull();
    expect(screen.queryByText("skills.model")).toBeNull();
  });
});
