import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  // Keep Rust compiler output visible when run through `tauri dev`.
  clearScreen: false,
  server: { port: 1420, strictPort: true },
});
