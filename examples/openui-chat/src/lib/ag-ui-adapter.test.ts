import { describe, expect, it } from "vitest";
import { awakenAgUiAdapter } from "./ag-ui-adapter";

function responseFromChunks(chunks: string[]): Response {
  const encoder = new TextEncoder();
  return new Response(
    new ReadableStream({
      start(controller) {
        for (const chunk of chunks) {
          controller.enqueue(encoder.encode(chunk));
        }
        controller.close();
      },
    }),
  );
}

async function collectEvents(response: Response) {
  const events = [];
  for await (const event of awakenAgUiAdapter().parse(response)) {
    events.push(event);
  }
  return events;
}

describe("awakenAgUiAdapter", () => {
  it("parses AG-UI SSE events split across network chunks", async () => {
    const response = responseFromChunks([
      'data: {"type":"TEXT_MESSAGE_CONTENT","delta":"hel',
      'lo"}\n\n',
    ]);

    await expect(collectEvents(response)).resolves.toEqual([
      { type: "TEXT_MESSAGE_CONTENT", delta: "hello" },
    ]);
  });

  it("parses multiple frames and ignores done markers", async () => {
    const response = responseFromChunks([
      'data: {"type":"RUN_STARTED","threadId":"t","runId":"r"}\n\n',
      "data: [DONE]\n\n",
      'data: {"type":"TEXT_MESSAGE_END","messageId":"m"}\n\n',
    ]);

    await expect(collectEvents(response)).resolves.toEqual([
      { type: "RUN_STARTED", threadId: "t", runId: "r" },
      { type: "TEXT_MESSAGE_END", messageId: "m" },
    ]);
  });

  it("parses CRLF-delimited SSE frames", async () => {
    const response = responseFromChunks([
      'data: {"type":"TEXT_MESSAGE_CONTENT","delta":"ok"}\r\n\r\n',
    ]);

    await expect(collectEvents(response)).resolves.toEqual([
      { type: "TEXT_MESSAGE_CONTENT", delta: "ok" },
    ]);
  });
});
