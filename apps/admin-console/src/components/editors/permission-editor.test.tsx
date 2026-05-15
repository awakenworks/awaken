// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { PermissionConfigEditor } from "./permission-editor";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function clickChoice(label: string, occurrence = 0) {
  const matches = screen.getAllByText(label);
  const target = matches.at(occurrence)?.closest("button");
  expect(target, `choice ${label}`).toBeTruthy();
  fireEvent.click(target!);
}

describe("PermissionConfigEditor", () => {
  it("serializes default decisions and newly added rules", () => {
    const onChange = vi.fn();
    render(
      <PermissionConfigEditor
        value={{ default_behavior: "ask", rules: [] }}
        onChange={onChange}
      />,
    );

    clickChoice("Allow");
    expect(onChange).toHaveBeenLastCalledWith({
      default_behavior: "allow",
      rules: [],
    });

    fireEvent.click(screen.getByRole("button", { name: "Add rule" }));
    expect(onChange).toHaveBeenLastCalledWith({
      default_behavior: "ask",
      rules: [{ tool: "", behavior: "ask", scope: "project" }],
    });
  });

  it("updates, reorders, and removes explicit permission rules", () => {
    const onChange = vi.fn();
    render(
      <PermissionConfigEditor
        value={{
          default_behavior: "deny",
          rules: [
            { tool: "Bash(npm *)", behavior: "ask", scope: "project" },
            { tool: "Read(docs/**)", behavior: "allow", scope: "session" },
          ],
        }}
        onChange={onChange}
      />,
    );

    fireEvent.change(screen.getByLabelText("Tool pattern"), {
      target: { value: "Bash(cargo test *)" },
    });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({ tool: "Bash(cargo test *)" }),
          expect.objectContaining({ tool: "Read(docs/**)" }),
        ],
      }),
    );

    clickChoice("Deny", -1);
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [expect.objectContaining({ behavior: "deny" }), expect.any(Object)],
      }),
    );

    clickChoice("Thread");
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [expect.objectContaining({ scope: "thread" }), expect.any(Object)],
      }),
    );

    fireEvent.click(screen.getByRole("button", { name: "Move down" }));
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({ tool: "Read(docs/**)" }),
          expect.objectContaining({ tool: "Bash(npm *)" }),
        ],
      }),
    );

    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [expect.objectContaining({ tool: "Bash(npm *)" })],
      }),
    );
  });

  it("renders any-tool rules with less common scopes and can promote the selected rule", () => {
    const onChange = vi.fn();
    render(
      <PermissionConfigEditor
        value={{
          default_behavior: "allow",
          rules: [
            { tool: "", behavior: "allow", scope: "once" },
            { tool: "Read(src/**)", behavior: "deny", scope: "user" },
          ],
        }}
        onChange={onChange}
      />,
    );

    expect(screen.getByText("(any tool)")).toBeTruthy();
    expect(screen.getByText("Scope · Once")).toBeTruthy();
    expect(screen.getByText("Scope · User")).toBeTruthy();

    fireEvent.click(screen.getByText("Read(src/**)").closest("button")!);
    fireEvent.click(screen.getByRole("button", { name: "Move up" }));

    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({ tool: "Read(src/**)", scope: "user" }),
          expect.objectContaining({ tool: "", scope: "once" }),
        ],
      }),
    );
  });

  it("removing the only rule emits an empty ruleset", () => {
    const onChange = vi.fn();
    render(
      <PermissionConfigEditor
        value={{
          default_behavior: "ask",
          rules: [{ tool: "Bash(*)", behavior: "ask", scope: "project" }],
        }}
        onChange={onChange}
      />,
    );

    expect(screen.getByRole("button", { name: "Move up" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByRole("button", { name: "Move down" }).hasAttribute("disabled")).toBe(true);

    fireEvent.click(screen.getByRole("button", { name: "Remove" }));

    expect(onChange).toHaveBeenLastCalledWith({
      default_behavior: "ask",
      rules: [],
    });
  });
});
