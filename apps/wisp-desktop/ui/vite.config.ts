import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// The built bundle is served inside the desktop app over the `wisp://`
// custom protocol, so asset URLs must stay relative to `index.html`.
export default defineConfig({
  plugins: [react(), tailwindcss()],
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
