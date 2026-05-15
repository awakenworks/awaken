// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { ReminderConfigEditor } from "./reminder-editor";

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

describe("ReminderConfigEditor", () => {
  it("serializes a newly added reminder rule", () => {
    const onChange = vi.fn();
    render(<ReminderConfigEditor value={{ rules: [] }} onChange={onChange} />);

    fireEvent.click(screen.getByRole("button", { name: "Add reminder" }));

    expect(onChange).toHaveBeenLastCalledWith({
      rules: [
        {
          name: undefined,
          tool: "",
          output: "any",
          message: { target: "system", content: "", cooldown_turns: 0 },
        },
      ],
    });
  });

  it("updates field matchers and reminder payload with serialized output", () => {
    const onChange = vi.fn();
    render(
      <ReminderConfigEditor
        value={{
          rules: [
            {
              name: "weather fields",
              tool: "get_weather",
              output: {
                content: {
                  fields: [{ path: "", op: "glob", value: "" }],
                },
              },
              message: {
                target: "system",
                content: "Bring an umbrella.",
                cooldown_turns: 0,
              },
            },
          ],
        }}
        onChange={onChange}
      />,
    );

    fireEvent.change(screen.getByLabelText("Path"), {
      target: { value: "error.code" },
    });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            output: {
              content: {
                fields: [expect.objectContaining({ path: "error.code" })],
              },
            },
          }),
        ],
      }),
    );

    fireEvent.change(screen.getByLabelText("Operation"), {
      target: { value: "exact" },
    });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            output: {
              content: {
                fields: [expect.objectContaining({ op: "exact" })],
              },
            },
          }),
        ],
      }),
    );

    fireEvent.change(screen.getByLabelText("Value"), {
      target: { value: "403" },
    });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            output: {
              content: {
                fields: [expect.objectContaining({ value: "403" })],
              },
            },
          }),
        ],
      }),
    );

    clickChoice("Conversation");
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            message: expect.objectContaining({ target: "conversation" }),
          }),
        ],
      }),
    );

    fireEvent.change(screen.getByLabelText("Reminder content"), {
      target: { value: "Escalate permission errors." },
    });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            message: expect.objectContaining({ content: "Escalate permission errors." }),
          }),
        ],
      }),
    );
  });

  it("updates status/text rules and preserves rule ordering operations", () => {
    const onChange = vi.fn();
    render(
      <ReminderConfigEditor
        value={{
          rules: [
            {
              name: "permission denied",
              tool: "Bash(*)",
              output: { status: "success", content: "*denied*" },
              message: { target: "system", content: "Check access.", cooldown_turns: 1 },
            },
            {
              name: "fallback",
              tool: "",
              output: "any",
              message: { target: "session", content: "Fallback reminder.", cooldown_turns: 0 },
            },
          ],
        }}
        onChange={onChange}
      />,
    );

    fireEvent.change(screen.getByLabelText("Rule name"), {
      target: { value: "permission failure" },
    });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [expect.objectContaining({ name: "permission failure" }), expect.any(Object)],
      }),
    );

    clickChoice("Error");
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            output: { status: "error", content: "*denied*" },
          }),
          expect.any(Object),
        ],
      }),
    );

    fireEvent.change(screen.getByLabelText("Content text matcher"), {
      target: { value: "*permission denied*" },
    });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            output: { status: "success", content: "*permission denied*" },
          }),
          expect.any(Object),
        ],
      }),
    );

    fireEvent.click(screen.getByRole("button", { name: "Move down" }));
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({ name: "fallback" }),
          expect.objectContaining({ name: "permission denied" }),
        ],
      }),
    );

    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [expect.objectContaining({ name: "permission denied" })],
      }),
    );
  });
});
