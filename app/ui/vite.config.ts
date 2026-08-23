import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The shell serves the built assets from disk, so everything must be relative.
export default defineConfig({
  plugins: [react()],
  base: "./",
  clearScreen: false,
  server: { port: 1420, strictPort: true },
  build: { target: "es2021", sourcemap: true },
});
