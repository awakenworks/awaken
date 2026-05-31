// @vitest-environment jsdom
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { AgentFrontendIntegrationCard } from "./agent-frontend-integration-card";

afterEach(cleanup);

describe("AgentFrontendIntegrationCard", () => {
  it("renders agent-scoped frontend protocol routes and docs links", () => {
    render(<AgentFrontendIntegrationCard agentId="support-agent" />);

    expect(screen.getByTestId("agent-frontend-integration-card").textContent).toContain(
      "/v1/ai-sdk/agents/support-agent/runs",
    );
    expect(screen.getByTestId("agent-frontend-integration-card").textContent).toContain(
      "/v1/ag-ui/agents/support-agent/runs",
    );
    expect(screen.getByRole("link", { name: "AI SDK guide" }).getAttribute("href")).toBe(
      "https://awakenworks.github.io/awaken/how-to/integrate-ai-sdk-frontend/",
    );
    expect(screen.getByRole("link", { name: "HTTP API" }).getAttribute("href")).toBe(
      "https://awakenworks.github.io/awaken/reference/http-api/",
    );
    expect(screen.getByTestId("agent-frontend-integration-card").textContent).toContain(
      "After the sandbox conversation behaves as expected",
    );
  });

  it("stays hidden until an agent has been saved", () => {
    const { container } = render(<AgentFrontendIntegrationCard agentId={undefined} />);

    expect(container.textContent).toBe("");
  });
});
