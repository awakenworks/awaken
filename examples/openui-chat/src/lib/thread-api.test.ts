import { describe, it, expect, vi, beforeEach } from "vitest";
import {
  fetchThreadList,
  loadThread,
  createThread,
  deleteThread,
  updateThread,
} from "./thread-api";

const mockFetch = vi.fn();
vi.stubGlobal("fetch", mockFetch);

beforeEach(() => {
  mockFetch.mockReset();
});

describe("fetchThreadList", () => {
  it("maps awaken summaries to OpenUI Thread format", async () => {
    mockFetch.mockResolvedValueOnce({
      ok: true,
      json: () =>
        Promise.resolve({
          items: [
            {
              id: "t1",
              title: "Hello",
              updated_at: 1700000000000,
              created_at: 1700000000000,
              message_count: 3,
              agent_id: "openui",
            },
          ],
        }),
    });

    const result = await fetchThreadList();

    expect(result.threads).toEqual([
      { id: "t1", title: "Hello", createdAt: 1700000000000 },
    ]);
  });

  it("returns empty list on fetch error", async () => {
    mockFetch.mockRejectedValueOnce(new Error("network"));

    const result = await fetchThreadList();

    expect(result.threads).toEqual([]);
  });
});

describe("loadThread", () => {
  it("flattens multimodal content arrays to strings", async () => {
    mockFetch.mockResolvedValueOnce({
      ok: true,
      json: () =>
        Promise.resolve({
          messages: [
            {
              id: "m1",
              role: "user",
              content: [{ type: "text", text: "hi" }],
            },
            {
              id: "m2",
              role: "assistant",
              content: [{ type: "text", text: "hello" }],
            },
          ],
        }),
    });

    const messages = await loadThread("t1");

    expect(messages).toHaveLength(2);
    expect(messages[0]).toMatchObject({ id: "m1", role: "user", content: "hi" });
    expect(messages[1]).toMatchObject({ id: "m2", role: "assistant", content: "hello" });
  });

  it("passes through string content unchanged", async () => {
    mockFetch.mockResolvedValueOnce({
      ok: true,
      json: () =>
        Promise.resolve({
          messages: [
            { id: "m1", role: "user", content: "plain text" },
          ],
        }),
    });

    const messages = await loadThread("t1");

    expect(messages[0]).toMatchObject({ id: "m1", content: "plain text" });
  });

  it("returns empty array on error", async () => {
    mockFetch.mockResolvedValueOnce({ ok: false });

    const messages = await loadThread("t1");

    expect(messages).toEqual([]);
  });
});

describe("createThread", () => {
  it("posts to /v1/threads and maps response to OpenUI Thread", async () => {
    mockFetch.mockResolvedValueOnce({
      ok: true,
      json: () =>
        Promise.resolve({
          id: "new-thread-id",
          metadata: { created_at: 1700000000000, title: null },
        }),
    });

    const firstMessage = { id: "m1", role: "user" as const, content: "hello" };
    const result = await createThread(firstMessage);

    expect(result).toEqual({
      id: "new-thread-id",
      title: "",
      createdAt: 1700000000000,
    });
    expect(mockFetch).toHaveBeenCalledWith(
      expect.stringContaining("/v1/threads"),
      expect.objectContaining({ method: "POST" }),
    );
  });
});

describe("deleteThread", () => {
  it("sends DELETE request", async () => {
    mockFetch.mockResolvedValueOnce({ ok: true });

    await deleteThread("t1");

    expect(mockFetch).toHaveBeenCalledWith(
      expect.stringContaining("/v1/threads/t1"),
      expect.objectContaining({ method: "DELETE" }),
    );
  });
});

describe("updateThread", () => {
  it("patches title and returns updated thread", async () => {
    mockFetch.mockResolvedValueOnce({ ok: true });

    const thread = { id: "t1", title: "New Title", createdAt: 1700000000000 };
    const result = await updateThread(thread);

    expect(result).toEqual(thread);
    expect(mockFetch).toHaveBeenCalledWith(
      expect.stringContaining("/v1/threads/t1/metadata"),
      expect.objectContaining({
        method: "PATCH",
        body: JSON.stringify({ title: "New Title" }),
      }),
    );
  });
});
