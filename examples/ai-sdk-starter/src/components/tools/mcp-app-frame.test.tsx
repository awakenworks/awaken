import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import "@testing-library/jest-dom/vitest";
import { McpAppFrame, getMcpUiContent } from "./mcp-app-frame";

describe("getMcpUiContent", () => {
  it("extracts metadata from valid tool output", () => {
    const output = {
      metadata: {
        "mcp.ui.content": "<p>Hello</p>",
        "mcp.ui.mimeType": "text/html",
        "mcp.ui.resourceUri": "ui://test/view",
      },
    };
    const result = getMcpUiContent(output);
    expect(result).toEqual({
      content: "<p>Hello</p>",
      mimeType: "text/html",
      resourceUri: "ui://test/view",
    });
  });

  it("returns null for output without metadata", () => {
    expect(getMcpUiContent({ data: { items: [1, 2] } })).toBeNull();
  });

  it("returns null for output with metadata but no mcp.ui.content", () => {
    expect(getMcpUiContent({ metadata: { other: "value" } })).toBeNull();
  });

  it("returns null for null/undefined output", () => {
    expect(getMcpUiContent(null)).toBeNull();
    expect(getMcpUiContent(undefined)).toBeNull();
  });

  it("returns null for non-object output", () => {
    expect(getMcpUiContent("string")).toBeNull();
    expect(getMcpUiContent(42)).toBeNull();
  });

  it("defaults mimeType to text/html when missing", () => {
    const output = {
      metadata: { "mcp.ui.content": "<div>Test</div>" },
    };
    const result = getMcpUiContent(output);
    expect(result?.mimeType).toBe("text/html");
    expect(result?.resourceUri).toBe("");
  });
});

describe("McpAppFrame", () => {
  it("renders iframe with content via srcdoc", () => {
    render(
      <McpAppFrame
        content="<p>Hello MCP</p>"
        mimeType="text/html"
        resourceUri="ui://test"
      />,
    );

    const wrapper = screen.getByTestId("mcp-app-frame");
    expect(wrapper).toBeInTheDocument();

    const iframe = screen.getByTestId("mcp-app-frame-iframe");
    expect(iframe).toBeInTheDocument();
    expect(iframe).toHaveAttribute("sandbox", "allow-scripts");
    expect(iframe.getAttribute("srcdoc")).toContain("<p>Hello MCP</p>");
  });

  it("displays resource URI label", () => {
    render(
      <McpAppFrame
        content="<p>Test</p>"
        resourceUri="ui://dashboard/view"
      />,
    );

    expect(screen.getByText("ui://dashboard/view")).toBeInTheDocument();
  });

  it("sets iframe title from resourceUri", () => {
    render(
      <McpAppFrame
        content="<p>Test</p>"
        resourceUri="ui://chart/render"
      />,
    );

    const iframe = screen.getByTestId("mcp-app-frame-iframe");
    expect(iframe).toHaveAttribute("title", "ui://chart/render");
  });

  it("uses default title when no resourceUri", () => {
    render(<McpAppFrame content="<p>Test</p>" />);

    const iframe = screen.getByTestId("mcp-app-frame-iframe");
    expect(iframe).toHaveAttribute("title", "MCP Application");
  });

  it("injects resize script into srcdoc", () => {
    render(<McpAppFrame content="<p>Content</p>" />);

    const iframe = screen.getByTestId("mcp-app-frame-iframe");
    const srcdoc = iframe.getAttribute("srcdoc") ?? "";
    expect(srcdoc).toContain("mcp-frame-resize");
    expect(srcdoc).toContain("ResizeObserver");
  });
});
