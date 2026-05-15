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
});
