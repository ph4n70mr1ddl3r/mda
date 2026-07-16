import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// During the spike, proxy /api and /health to the local Rust server.
export default defineConfig({
  plugins: [react()],
  server: { proxy: { "/api": "http://localhost:8080", "/health": "http://localhost:8080" } },
});
