import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The UI is served by `mineral web` from `dist/`, same origin as the API, so
// there is no CORS anywhere and no API base URL to configure. During
// development the Vite dev server proxies `/api` to a running `mineral web`,
// which keeps the same-origin property in development too.
export default defineConfig({
  plugins: [react()],
  server: {
    // Loopback only, like the API it proxies to.
    host: "127.0.0.1",
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8787",
        changeOrigin: false,
        // SSE must not be buffered by the proxy.
        ws: false,
      },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // Absolute asset URLs, so a page served at /operations/op-17 still resolves
    // /assets/... correctly.
    assetsDir: "assets",
  },
});
