// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Button } from "./button";
import { Input, Textarea } from "./input";

afterEach(() => cleanup());

describe("Button", () => {
  it("defaults to type=button to avoid surprise form submits", () => {
    render(<Button>x</Button>);
    expect(screen.getByRole("button").getAttribute("type")).toBe("button");
  });

  it("disabled when loading", () => {
    render(<Button loading>save</Button>);
    expect((screen.getByRole("button") as HTMLButtonElement).disabled).toBe(true);
  });

  it("variant adds class hooks", () => {
    const { rerender } = render(<Button variant="primary">x</Button>);
    expect(screen.getByRole("button").className).toMatch(/bg-accent/);
    rerender(<Button variant="danger">x</Button>);
    expect(screen.getByRole("button").className).toMatch(/bg-tone-error/);
  });

  it("fires onClick", () => {
    const handle = vi.fn();
    render(<Button onClick={handle}>x</Button>);
    fireEvent.click(screen.getByRole("button"));
    expect(handle).toHaveBeenCalled();
  });
});

describe("Input + Textarea", () => {
  it("Input forwards ref + props", () => {
    render(<Input placeholder="email" defaultValue="x" />);
    const el = screen.getByPlaceholderText("email") as HTMLInputElement;
    expect(el.value).toBe("x");
    expect(el.className).toMatch(/border-line-strong/);
  });

  it("Input mono adds font-mono", () => {
    render(<Input mono placeholder="id" />);
    expect(screen.getByPlaceholderText("id").className).toMatch(/font-mono/);
  });

  it("Textarea respects rows + mono", () => {
    render(<Textarea mono rows={6} placeholder="json" />);
    const ta = screen.getByPlaceholderText("json") as HTMLTextAreaElement;
    expect(ta.rows).toBe(6);
    expect(ta.className).toMatch(/font-mono/);
  });
});
