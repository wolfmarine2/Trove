import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// During development, API and repository traffic goes to a local Trove server.
const backend = process.env.TROVE_BACKEND ?? "http://127.0.0.1:8081";

export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": backend,
      "/repository": backend,
      "/metrics": backend,
    },
  },
  build: {
    outDir: "dist",
    sourcemap: false,
    chunkSizeWarningLimit: 800,
  },
});
