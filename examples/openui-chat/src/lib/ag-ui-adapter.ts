import type { AGUIEvent, StreamProtocolAdapter } from "@openuidev/react-headless";

function* parseSseFrame(frame: string): Generator<AGUIEvent> {
  const data = frame
    .split(/\r?\n/)
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trimStart())
    .join("\n")
    .trim();

  if (!data || data === "[DONE]") {
    return;
  }

  try {
    yield JSON.parse(data) as AGUIEvent;
  } catch (error) {
    console.error("Failed to parse AG-UI SSE event", error);
  }
}

function findFrameDelimiter(buffer: string): { index: number; length: number } | null {
  const lf = buffer.indexOf("\n\n");
  const crlf = buffer.indexOf("\r\n\r\n");

  if (lf === -1 && crlf === -1) {
    return null;
  }

  if (lf === -1) {
    return { index: crlf, length: 4 };
  }

  if (crlf === -1 || lf < crlf) {
    return { index: lf, length: 2 };
  }

  return { index: crlf, length: 4 };
}

export function awakenAgUiAdapter(): StreamProtocolAdapter {
  return {
    async *parse(response: Response): AsyncIterable<AGUIEvent> {
      const reader = response.body?.getReader();
      if (!reader) {
        throw new Error("No response body");
      }

      const decoder = new TextDecoder();
      let buffer = "";

      while (true) {
        const { done, value } = await reader.read();
        if (done) {
          break;
        }

        buffer += decoder.decode(value, { stream: true });

        let delimiter = findFrameDelimiter(buffer);
        while (delimiter) {
          const frame = buffer.slice(0, delimiter.index);
          buffer = buffer.slice(delimiter.index + delimiter.length);
          yield* parseSseFrame(frame);
          delimiter = findFrameDelimiter(buffer);
        }
      }

      buffer += decoder.decode();
      if (buffer.trim()) {
        yield* parseSseFrame(buffer);
      }
    },
  };
}
