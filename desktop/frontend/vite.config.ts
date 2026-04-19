import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri v2 expects the built assets under `desktop/dist/` (see tauri.conf.json
// -> build.frontendDist = "../dist"). We live under `desktop/frontend/` so the
// build target is one level up in `../dist`.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    target: "es2021",
    sourcemap: true,
  },
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: "127.0.0.1",
  },
  envPrefix: ["VITE_", "TAURI_"],
});
