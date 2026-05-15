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

    fireEvent.change(screen.getByLabelText("Cooldown turns"), {
      target: { value: "3" },
    });
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            message: expect.objectContaining({ cooldown_turns: 3 }),
          }),
        ],
      }),
    );

    fireEvent.click(screen.getAllByRole("button", { name: "Remove" }).at(-1)!);
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.objectContaining({
            output: { content: { fields: [] } },
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

  it("renders reminder mode labels and summarizes more than two targets", () => {
    const onChange = vi.fn();
    render(
      <ReminderConfigEditor
        value={{
          rules: [
            {
              name: "status",
              tool: "job_status",
              output: { status: "pending" },
              message: { target: "system", content: "Wait.", cooldown_turns: 0 },
            },
            {
              name: "text",
              tool: "Bash(*)",
              output: { content: "*denied*" },
              message: { target: "suffix_system", content: "Check permissions.", cooldown_turns: 1 },
            },
            {
              name: "fields",
              tool: "get_weather",
              output: { content: { fields: [] } },
              message: { target: "session", content: "Remember weather.", cooldown_turns: 2 },
            },
            {
              name: "status fields",
              tool: "api_call",
              output: { status: "error", content: { fields: [] } },
              message: { target: "conversation", content: "Inspect response.", cooldown_turns: 3 },
            },
          ],
        }}
        onChange={onChange}
      />,
    );

    expect(screen.getByText("System, Suffix system +2")).toBeTruthy();
    expect(screen.getByText("Status")).toBeTruthy();
    expect(screen.getByText("Text")).toBeTruthy();
    expect(screen.getByText("Fields")).toBeTruthy();
    expect(screen.getAllByText("Status + fields").length).toBeGreaterThan(0);

    fireEvent.click(screen.getByText("fields").closest("button")!);
    expect(screen.getByText("No field conditions configured yet.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Add field condition" }));
    expect(onChange).toHaveBeenLastCalledWith(
      expect.objectContaining({
        rules: [
          expect.any(Object),
          expect.any(Object),
          expect.objectContaining({
            output: {
              content: {
                fields: [expect.objectContaining({ path: "", op: "glob", value: "" })],
              },
            },
          }),
          expect.any(Object),
        ],
      }),
    );
  });
});
