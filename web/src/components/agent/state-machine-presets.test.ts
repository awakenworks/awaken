import { describe, expect, it } from "vitest";
import {
  PRESETS,
  fromList,
  machineStates,
  triggerKind,
  triggerText,
  withTriggerKind,
} from "./state-machine-presets";

describe("state-machine presets", () => {
  it("ships the three user intents", () => {
    expect(PRESETS.map((preset) => preset.key)).toEqual([
      "read-before-write",
      "background-task-reminder",
      "todo-reminder",
    ]);
    for (const preset of PRESETS) {
      expect(preset.hint.length).toBeGreaterThan(20);
      expect(preset.value.length).toBeGreaterThan(20);
    }
  });

  it("every machine references a consistent state set", () => {
    for (const preset of PRESETS) {
      const machine = preset.machine;
      const states = new Set(machineStates(machine));
      expect(states.has(machine.initial)).toBe(true);
      for (const state of machine.terminal ?? []) expect(states.has(state)).toBe(true);
      for (const transition of machine.transitions) {
        expect(states.has(transition.to)).toBe(true);
        for (const from of fromList(transition.from)) expect(states.has(from)).toBe(true);
      }
    }
  });

  it("read-before-write blocks before execution and permits repeated writes", () => {
    const machine = PRESETS[0].machine;
    expect(machine.scope).toBe("thread");
    expect(machine.key).toBe("{path}");
    expect(machine.key_normalizer).toBe("path");
    const write = machine.transitions.find((transition) => triggerText(transition.on).startsWith("write"))!;
    expect(fromList(write.from)).toEqual(["read", "written"]);
    expect(write.when).toEqual({ status: "success" });
    expect(write.on_violation?.action).toBe("deny");
  });

  it("reminder presets use lifecycle facts and request-only context", () => {
    for (const key of ["background-task-reminder", "todo-reminder"]) {
      const machine = PRESETS.find((preset) => preset.key === key)!.machine;
      const reminder = machine.transitions.find((transition) => transition.emit)!;
      expect(triggerKind(reminder.on)).toBe("event");
      expect(triggerText(reminder.on)).toBe("step.before_inference");
      expect(reminder.emit?.target).toBe("context");
      expect(reminder.emit?.cooldown_steps).toBeGreaterThan(0);
      expect(Object.keys(reminder.counters ?? {})).toContain("steps");
    }
  });

  it("switching a trigger to event removes tool-only fields and forces context", () => {
    const transition = withTriggerKind(
      {
        on: "write",
        from: "read",
        to: "written",
        when: "success",
        on_violation: { action: "ask" },
        emit: { target: "conversation", content: "note" },
      },
      "event",
    );
    expect(triggerKind(transition.on)).toBe("event");
    expect(transition.when).toBeUndefined();
    expect(transition.on_violation).toBeUndefined();
    expect(transition.emit?.target).toBe("context");
  });

  it("machineStates collects initial, terminal, from and to", () => {
    expect(new Set(machineStates(PRESETS[0].machine))).toEqual(
      new Set(["unread", "read", "written"]),
    );
  });
});
