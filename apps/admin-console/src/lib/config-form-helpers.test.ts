import { describe, expect, it } from "vitest";
import {
  parseJsonObject,
  parseLineList,
  parseStringRecord,
  stringifyJsonObject,
  stringifyLineList,
} from "./config-form-helpers";

describe("config form helpers", () => {
  it("parses newline separated lists", () => {
    expect(parseLineList(" alpha\n\nbravo \ncharlie")).toEqual([
      "alpha",
      "bravo",
      "charlie",
    ]);
  });

  it("stringifies newline separated lists", () => {
    expect(stringifyLineList(["alpha", "bravo"])).toBe("alpha\nbravo");
  });

  it("parses json objects", () => {
    expect(parseJsonObject<{ mode: string }>('{"mode":"strict"}', "Config")).toEqual(
      { mode: "strict" },
    );
  });

  it("rejects non-object json", () => {
    expect(() => parseJsonObject("[]", "Config")).toThrow(
      "Config must be a JSON object",
    );
  });

  it("parses string records", () => {
    expect(parseStringRecord('{"TOKEN":"secret"}', "Environment")).toEqual({
      TOKEN: "secret",
    });
  });

  it("rejects non-string record values", () => {
    expect(() => parseStringRecord('{"TOKEN":1}', "Environment")).toThrow(
      'Environment value for "TOKEN" must be a string',
    );
  });

  it("stringifies empty objects as braces", () => {
    expect(stringifyJsonObject(undefined)).toBe("{}");
  });
});
