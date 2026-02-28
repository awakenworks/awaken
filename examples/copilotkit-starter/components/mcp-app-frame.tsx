"use client";

import { useEffect, useRef, useState } from "react";

type McpUiContent = {
  content: string;
  mimeType: string;
  resourceUri: string;
};

export function getMcpUiContent(output: unknown): McpUiContent | null {
  if (!output || typeof output !== "object") return null;
  const obj = output as Record<string, unknown>;
  const metadata = obj.metadata;
  if (!metadata || typeof metadata !== "object") return null;
  const meta = metadata as Record<string, unknown>;
  const content = meta["mcp.ui.content"];
  if (typeof content !== "string") return null;
  return {
    content,
    mimeType: typeof meta["mcp.ui.mimeType"] === "string" ? (meta["mcp.ui.mimeType"] as string) : "text/html",
    resourceUri: typeof meta["mcp.ui.resourceUri"] === "string" ? (meta["mcp.ui.resourceUri"] as string) : "",
  };
}

const RESIZE_SCRIPT = `
<script>
(function() {
  function postHeight() {
    var h = document.documentElement.scrollHeight || document.body.scrollHeight;
    window.parent.postMessage({ type: 'mcp-frame-resize', height: h }, '*');
  }
  window.addEventListener('load', postHeight);
  if (typeof ResizeObserver !== 'undefined') {
    new ResizeObserver(postHeight).observe(document.body);
  }
  postHeight();
})();
</script>`;

type McpAppFrameProps = {
  content: string;
  mimeType?: string;
  resourceUri?: string;
};

export function McpAppFrame({ content, mimeType, resourceUri }: McpAppFrameProps) {
  const [height, setHeight] = useState(400);
  const [loaded, setLoaded] = useState(false);
  const iframeRef = useRef<HTMLIFrameElement>(null);

  useEffect(() => {
    function onMessage(e: MessageEvent) {
      if (e.data?.type === "mcp-frame-resize" && typeof e.data.height === "number") {
        setHeight(Math.min(e.data.height, 600));
      }
    }
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, []);

  const srcdoc = content + RESIZE_SCRIPT;

  return (
    <div data-testid="mcp-app-frame" className="relative my-2 rounded-xl border border-slate-300 bg-white/90 shadow-sm overflow-hidden">
      {!loaded && (
        <div className="absolute inset-0 flex items-center justify-center bg-white/80 text-sm text-slate-500">
          Loading MCP app...
        </div>
      )}
      <iframe
        ref={iframeRef}
        data-testid="mcp-app-frame-iframe"
        srcDoc={srcdoc}
        sandbox="allow-scripts"
        style={{ height: `${height}px`, width: "100%", border: "none", display: "block" }}
        onLoad={() => setLoaded(true)}
        title={resourceUri || "MCP Application"}
      />
      {resourceUri && (
        <div className="border-t border-slate-200 bg-slate-50 px-3 py-1 text-xs text-slate-400 truncate">
          {resourceUri}
        </div>
      )}
    </div>
  );
}
