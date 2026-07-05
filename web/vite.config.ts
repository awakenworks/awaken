import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// The console talks to awaken-server (management mode). Every API path
// is proxied verbatim — the client's paths ARE the wire paths, so the dev
// proxy is pure passthrough and production can serve the SPA from the same
// origin as the API.
const BACKEND = process.env.AWAKEN_HTTP_URL ?? "http://127.0.0.1:38080";

export default defineConfig({
  plugins: [react()],
  server: {
    host: "127.0.0.1",
    port: 3002,
    proxy: {
      "/v1": { target: BACKEND, changeOrigin: true },
      "/projects": { target: BACKEND, changeOrigin: true },
    },
  },
});
