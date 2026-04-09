import react from "@vitejs/plugin-react";
import { resolve } from "path";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": resolve(__dirname, "src"),
    },
  },
  server: {
    host: "127.0.0.1",
    port: 3002,
  },
  preview: {
    host: "127.0.0.1",
    port: 3002,
  },
  test: {
    environment: "node",
    globals: true,
  },
});
