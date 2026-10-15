import { defineConfig } from "vite";
import { fileURLToPath } from "node:url";

// root が src のため、エントリは絶対パスで与える (launcher = index.html, capture = capture.html)。
const entry = (p: string): string => fileURLToPath(new URL(p, import.meta.url));

export default defineConfig({
  root: "src",
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    target: "es2022",
    rollupOptions: {
      input: {
        main: entry("src/index.html"),
        capture: entry("src/capture.html"),
        mobile: entry("src/mobile.html"),
        "tab-ghost": entry("src/tab-ghost.html"),
      },
    },
  },
  server: {
    port: 5173,
    strictPort: true,
  },
  clearScreen: false,
});
