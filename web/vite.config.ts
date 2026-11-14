import { writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

const backend = "http://127.0.0.1:7311";
// Anchored patterns: a plain "/f" prefix would also swallow client routes like /flows and /forms.
const proxied = ["api", "client", "t", "u", "f", "mcp", "webhooks", "healthz"].map((p) => `^/${p}(/|$|\\?)`);

// The repo tracks dist/.placeholder.html so the Rust embed compiles before the UI is built;
// emptyOutDir wipes it, so write it back after each build.
const placeholder =
  '<!doctype html><title>Postlark</title><p>UI not built. Run <code>pnpm -C web build</code>.</p>\n';
function keepPlaceholder(): Plugin {
  return {
    name: "postlark-keep-placeholder",
    apply: "build",
    closeBundle() {
      writeFileSync(resolve(import.meta.dirname, "dist/.placeholder.html"), placeholder);
    },
  };
}

// Vite requires a default export for its config file.
export default defineConfig({
  plugins: [react(), tailwindcss(), keepPlaceholder()],
  server: {
    host: true,
    port: 7312,
    strictPort: true,
    proxy: Object.fromEntries(proxied.map((path) => [path, { target: backend, changeOrigin: false }])),
  },
  build: { outDir: "dist", emptyOutDir: true },
});
