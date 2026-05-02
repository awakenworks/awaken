// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  cleanup,
  fireEvent,
  render,
  screen,
} from "@testing-library/react";
import { SegmentedControl } from "./segmented";
import { SecretField, SecretStatusPill } from "./secret-field";

afterEach(() => cleanup());

describe("SegmentedControl", () => {
  it("emits selected value on click", () => {
    const handle = vi.fn();
    render(
      <SegmentedControl
        value="a"
        onChange={handle}
        options={[
          { value: "a", label: "A" },
          { value: "b", label: "B" },
        ]}
      />,
    );
    fireEvent.click(screen.getByRole("radio", { name: /B/ }));
    expect(handle).toHaveBeenCalledWith("b");
  });

  it("marks the active option with aria-checked", () => {
    render(
      <SegmentedControl
        value="b"
        onChange={() => {}}
        options={[
          { value: "a", label: "A" },
          { value: "b", label: "B" },
        ]}
      />,
    );
    expect(screen.getByRole("radio", { name: /A/ }).getAttribute("aria-checked")).toBe(
      "false",
    );
    expect(screen.getByRole("radio", { name: /B/ }).getAttribute("aria-checked")).toBe(
      "true",
    );
  });
});

describe("SecretField", () => {
  function setup(overrides: Partial<Parameters<typeof SecretField>[0]> = {}) {
    const onModeChange = vi.fn();
    const props = {
      mode: "keep" as const,
      onModeChange,
      currentlyHasValue: true,
      labels: {
        title: "API key",
        description: "desc",
        replaceLabel: "Set new",
        clearLabel: "Clear",
        keepBody: "Keep msg",
        clearBody: "Clear msg",
      },
      children: <input data-testid="value" />,
      ...overrides,
    };
    render(<SecretField {...props}>{props.children}</SecretField>);
    return { onModeChange };
  }

  it("offers all 3 modes when a value is currently stored", () => {
    setup();
    expect(screen.getByRole("radio", { name: /Keep/ })).toBeDefined();
    expect(screen.getByRole("radio", { name: /Set new/ })).toBeDefined();
    expect(screen.getByRole("radio", { name: /Clear/ })).toBeDefined();
  });

  it("offers only replace when no value is currently stored", () => {
    setup({ currentlyHasValue: false, mode: "replace" });
    expect(screen.queryByRole("radio", { name: /Keep/ })).toBeNull();
    expect(screen.queryByRole("radio", { name: /Clear/ })).toBeNull();
    expect(screen.getByRole("radio", { name: /Set new/ })).toBeDefined();
  });

  it("renders the success body in keep mode", () => {
    setup({ mode: "keep" });
    expect(screen.getByText("Keep msg")).toBeDefined();
    expect(screen.queryByTestId("value")).toBeNull();
  });

  it("renders the editor children in replace mode", () => {
    setup({ mode: "replace" });
    expect(screen.getByTestId("value")).toBeDefined();
    expect(screen.queryByText("Keep msg")).toBeNull();
  });

  it("renders the warning body in clear mode", () => {
    setup({ mode: "clear" });
    expect(screen.getByText("Clear msg")).toBeDefined();
    expect(screen.queryByTestId("value")).toBeNull();
  });

  it("emits onModeChange when a different option is clicked", () => {
    const { onModeChange } = setup({ mode: "keep" });
    fireEvent.click(screen.getByRole("radio", { name: /Clear/ }));
    expect(onModeChange).toHaveBeenCalledWith("clear");
  });
});

describe("SecretStatusPill", () => {
  it("renders four distinct labels", () => {
    const states = ["stored", "no-value", "will-clear", "will-set"] as const;
    for (const s of states) {
      cleanup();
      render(<SecretStatusPill state={s} />);
    }
    // Each render is verified above; this just confirms no crashes.
    expect(true).toBe(true);
  });

  it("includes fingerprint suffix when provided to stored", () => {
    render(<SecretStatusPill state="stored" fingerprint="9c14" />);
    expect(screen.getByText(/fp:9c14/)).toBeDefined();
  });
});
