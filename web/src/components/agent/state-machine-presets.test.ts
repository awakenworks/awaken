import { describe, expect, it } from "vitest";
import { PRESETS, fromList, machineStates } from "./state-machine-presets";

describe("state-machine presets", () => {
  it("ships the three canonical shapes", () => {
    expect(PRESETS.map((p) => p.key)).toEqual([
      "read-before-write",
      "background-task-reminder",
      "todo-reminder",
    ]);
  });

  it("every preset machine is internally consistent (initial/terminal/to are real states)", () => {
    for (const p of PRESETS) {
      const m = p.machine;
      const states = new Set(machineStates(m));
      expect(states.has(m.initial)).toBe(true);
      for (const s of m.terminal ?? []) expect(states.has(s)).toBe(true);
      for (const t of m.transitions) {
        expect(states.has(t.to)).toBe(true);
        for (const f of fromList(t.from)) expect(states.has(f)).toBe(true);
      }
    }
  });

  it("read-before-write denies an unread write and keys per file", () => {
    const m = PRESETS[0].machine;
    expect(m.key).toBe("{path}");
    const write = m.transitions.find((t) => t.on.startsWith("write"))!;
    expect(write.on_violation?.action).toBe("deny");
  });

  it("the reminder presets emit a suffix-system reminder with a cooldown", () => {
    for (const key of ["background-task-reminder", "todo-reminder"]) {
      const m = PRESETS.find((p) => p.key === key)!.machine;
      const emit = m.transitions[0].emit!;
      expect(emit.target).toBe("suffix_system");
      expect(emit.content.length).toBeGreaterThan(10);
      expect(emit.cooldown_turns).toBeGreaterThan(0);
    }
  });

  it("machineStates collects initial + terminal + every from/to", () => {
    expect(new Set(machineStates(PRESETS[0].machine))).toEqual(
      new Set(["unread", "read", "written"]),
    );
  });
});
