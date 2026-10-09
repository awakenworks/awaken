import { brandAppearancePlugin, brandFaviconPlugin } from "@awaken/ui/brand/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The console talks to awaken-server (management mode). Every API path
// is proxied verbatim — the client's paths ARE the wire paths, so the dev
// proxy is pure passthrough and production can serve the SPA from the same
// origin as the API.
const BACKEND = process.env.AWAKEN_HTTP_URL ?? "http://127.0.0.1:38080";
const backendOrigin = new URL(BACKEND).origin;
const apiProxy = () => ({
  target: BACKEND,
  changeOrigin: true,
  // Self-managed local sign-in validates both Host and Origin. The browser's
  // dev-server Origin is not the API authority, so project it to the same
  // backend origin that production gets naturally from its same-origin SPA.
  configure(proxy: { on: (event: "proxyReq", listener: (request: { setHeader: (name: string, value: string) => void }) => void) => void }) {
    proxy.on("proxyReq", (request) => request.setHeader("origin", backendOrigin));
  },
});

export default defineConfig({
  // Static assets come from imports and the brand plugins. A missing public
  // directory cannot be a Cargo build input: it makes every build stale.
  publicDir: false,
  plugins: [react(), brandFaviconPlugin("agents"), brandAppearancePlugin("agents", { legacyStorageKeys: ["awaken.console.theme"] })],
  build: {
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (!id.includes("node_modules")) return undefined;
          if (id.includes("/@tanstack/")) return "query-vendor";
          if (id.includes("/react-router/")) return "router-vendor";
          if (id.includes("/@ai-sdk/") || id.includes("/ai/")) return "assistant-vendor";
          if (id.includes("/react/") || id.includes("/react-dom/") || id.includes("/scheduler/")) return "react-vendor";
          return undefined;
        },
      },
    },
  },
  resolve: {
    dedupe: ["react", "react-dom"],
  },
  server: {
    host: "127.0.0.1",
    port: 3002,
    proxy: {
      "/.well-known": apiProxy(),
      "/v1": apiProxy(),
      "/projects": apiProxy(),
    },
  },
});
