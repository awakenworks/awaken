// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import {
  TimeRangeSwitcher,
  TIME_RANGE_SECONDS,
  type TimeRange,
} from "./time-range-switcher";

afterEach(() => cleanup());

describe("TimeRangeSwitcher", () => {
  it("renders 4 default option buttons in a radiogroup", () => {
    render(<TimeRangeSwitcher value="24h" onChange={() => {}} />);
    const group = screen.getByRole("radiogroup");
    expect(group).toBeDefined();
    const radios = screen.getAllByRole("radio");
    expect(radios).toHaveLength(4);
    expect(radios.map((r) => r.textContent)).toEqual(["15m", "1h", "24h", "7d"]);
  });

  it("marks the active option with aria-checked", () => {
    render(<TimeRangeSwitcher value="1h" onChange={() => {}} />);
    const radios = screen.getAllByRole("radio");
    expect(radios[1].getAttribute("aria-checked")).toBe("true");
    expect(radios[0].getAttribute("aria-checked")).toBe("false");
    expect(radios[2].getAttribute("aria-checked")).toBe("false");
  });

  it("fires onChange with the clicked range", () => {
    const handle = vi.fn();
    render(<TimeRangeSwitcher value="24h" onChange={handle} />);
    fireEvent.click(screen.getByRole("radio", { name: "7d" }));
    expect(handle).toHaveBeenCalledWith("7d");
  });

  it("respects custom options list", () => {
    const opts: TimeRange[] = ["1h", "24h", "30d"];
    render(<TimeRangeSwitcher value="24h" onChange={() => {}} options={opts} />);
    const radios = screen.getAllByRole("radio");
    expect(radios.map((r) => r.textContent)).toEqual(["1h", "24h", "30d"]);
  });
});

describe("TIME_RANGE_SECONDS", () => {
  it("maps each preset to seconds correctly", () => {
    expect(TIME_RANGE_SECONDS["15m"]).toBe(900);
    expect(TIME_RANGE_SECONDS["1h"]).toBe(3600);
    expect(TIME_RANGE_SECONDS["24h"]).toBe(86400);
    expect(TIME_RANGE_SECONDS["7d"]).toBe(604800);
    expect(TIME_RANGE_SECONDS["30d"]).toBe(2592000);
  });
});
