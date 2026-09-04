import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The built bundle is served inside the desktop app from the custom
// `wisp://` scheme, so asset URLs must stay relative to `index.html`.
export default defineConfig({
  plugins: [react()],
  base: "./",
  server: {
    port: 5183,
    strictPort: true,
  },
  build: {
    outDir: "dist",
    target: "safari16",
  },
});
