// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useTheme } from "./use-theme";

beforeEach(() => {
  window.localStorage.clear();
  document.documentElement.removeAttribute("data-theme");
});
afterEach(() => {
  window.localStorage.clear();
});

describe("useTheme", () => {
  it("defaults to system when no localStorage entry exists", () => {
    const { result } = renderHook(() => useTheme());
    expect(result.current.choice).toBe("system");
  });

  it("reads stored choice from localStorage on mount", () => {
    window.localStorage.setItem("awaken.admin.theme", "dark");
    const { result } = renderHook(() => useTheme());
    expect(result.current.choice).toBe("dark");
  });

  it("setChoice updates state, localStorage, and html data-theme attribute", () => {
    const { result } = renderHook(() => useTheme());
    act(() => result.current.setChoice("dark"));
    expect(result.current.choice).toBe("dark");
    expect(window.localStorage.getItem("awaken.admin.theme")).toBe("dark");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
  });

  it("setChoice('system') clears the data-theme attribute", () => {
    document.documentElement.setAttribute("data-theme", "dark");
    const { result } = renderHook(() => useTheme());
    act(() => result.current.setChoice("system"));
    expect(document.documentElement.getAttribute("data-theme")).toBeNull();
  });

  it("cycle progresses light → dark → system → light", () => {
    window.localStorage.setItem("awaken.admin.theme", "light");
    const { result } = renderHook(() => useTheme());
    expect(result.current.choice).toBe("light");
    act(() => result.current.cycle());
    expect(result.current.choice).toBe("dark");
    act(() => result.current.cycle());
    expect(result.current.choice).toBe("system");
    act(() => result.current.cycle());
    expect(result.current.choice).toBe("light");
  });
});
