import { defineConfig } from "vite";
import { rnw } from "vite-plugin-rnw";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const r = (m: string) => require.resolve(m);

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// vite-plugin-rnw wraps @vitejs/plugin-react and handles react-native -> react-native-web.
// Tamagui runs at runtime on web (optional compiler omitted); TAMAGUI_TARGET=web lets
// @tamagui/core register/read config on the web global.
//
// React lives in desktop/node_modules (the workspace root has none, to keep mobile on a single
// React instance), so pin every `react`/`react-dom` import to desktop's copy — otherwise the
// root-level packages (tamagui, react-native-web) fail to resolve `react` during dep pre-bundle.
// https://vite.dev/config/
export default defineConfig({
  plugins: [rnw({ jsxRuntime: "automatic" })],
  define: {
    "process.env.TAMAGUI_TARGET": JSON.stringify("web"),
  },
  resolve: {
    dedupe: ["react", "react-dom"],
    // Exact-match (regex) so the bare `react` alias does not also rewrite `react/jsx-runtime` etc.
    alias: [
      { find: /^react$/, replacement: r("react") },
      { find: /^react-dom$/, replacement: r("react-dom") },
      { find: /^react-dom\/client$/, replacement: r("react-dom/client") },
      { find: /^react\/jsx-runtime$/, replacement: r("react/jsx-runtime") },
      { find: /^react\/jsx-dev-runtime$/, replacement: r("react/jsx-dev-runtime") },
    ],
  },
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
