import { describe, expect, it } from "vitest";
import type { InputBinding, ResourceInputDefaultMounts } from "./api/types";
import {
  createDefaultResourceBinding,
  switchResourceBindingKind,
} from "./resource-input-defaults";

const defaults: ResourceInputDefaultMounts = {
  file: "/mnt/files/data",
  memory_store: "/mnt/memory",
  repository: "/workspace/repo",
};

describe("Resource input default capability cause/effect table", () => {
  it("creates and switches every kind only from the capability projection", () => {
    // | Rule | capability | operation | kind | Effect |
    // | W1 | present | add | File/Memory/Repository | projected path + typed access |
    // | W2 | present | switch | Repository -> File | projected File path + forced RO |
    // The input binding id remains stable across a switch; no local path table exists.
    expect(createDefaultResourceBinding(defaults, "file", "file-binding", "")).toMatchObject({
      mount_path: "/mnt/files/data",
      access: "read_only",
    });
    expect(createDefaultResourceBinding(defaults, "memory_store", "memory-binding", "memory-1")).toMatchObject({
      mount_path: "/mnt/memory",
      access: "read_write",
    });
    const repository = createDefaultResourceBinding(defaults, "repository", "repo-binding", "repo-1");
    expect(repository).toMatchObject({
      mount_path: "/workspace/repo",
      access: "read_only",
    });
    expect(switchResourceBindingKind(repository!, defaults, "file")).toEqual({
      binding_id: "repo-binding",
      target: { kind: "file", id: "" },
      mount_path: "/mnt/files/data",
      access: "read_only",
    });
  });

  it("fails closed without capability data and leaves a reloaded binding untouched", () => {
    // | Rule | capability | operation | Effect |
    // | W3 | absent | add/switch | no candidate binding; caller cannot invent a path |
    // | W4 | absent | reload existing explicit binding | exact durable value retained |
    const reloaded: InputBinding = {
      binding_id: "persisted",
      target: { kind: "repository", id: "repo-1" },
      mount_path: "/workspace/custom",
      access: "read_write",
    };
    expect(createDefaultResourceBinding(undefined, "repository", "new", "")).toBeUndefined();
    expect(switchResourceBindingKind(reloaded, undefined, "file")).toBeUndefined();
    expect(reloaded.mount_path).toBe("/workspace/custom");
    expect(reloaded.target.kind).toBe("repository");
  });
});
