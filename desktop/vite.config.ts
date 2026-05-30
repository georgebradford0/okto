import { defineConfig } from "vite";
import { rnw } from "vite-plugin-rnw";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";

const require = createRequire(import.meta.url);
// Absolute path to @legendapp/motion's compiled ESM entry. We resolve it via package.json
// (the only path its `exports` map exposes) because an absolute path bypasses that exports map
// — importing the subpath by specifier is blocked ("Missing ./lib/module/index.js specifier").
const legendMotionEsm = join(
  dirname(require.resolve("@legendapp/motion/package.json")),
  "lib/module/index.js",
);

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// vite-plugin-rnw wraps @vitejs/plugin-react and injects the react-native -> react-native-web
// alias, web-first resolve.extensions, RN `define` globals, and the esbuild flow/jsx loaders.
// We route JSX through nativewind so className styling is applied; the gluestack className JSX
// lives in @okto/ui source (a linked workspace package, transformed since it's not under
// node_modules), while @gluestack-ui's own packages are pre-bundled by esbuild.
// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [
    rnw({
      jsxImportSource: "nativewind",
      jsxRuntime: "automatic",
    }),
  ],

  resolve: {
    // react-native-web pulls inline-style-prefixer's CJS `lib/` build, whose `exports.default`
    // is dropped when the rnw plugin's esbuild flow/jsx loader parses it as a module. The package
    // ships a parallel ESM `es/` build (real `export default`) that survives that loader, so map
    // the deep CJS imports onto it. Fixes the dev "no export named 'default'" optimizer failures.
    alias: [
      {
        find: /^inline-style-prefixer\/lib\/(.*)$/,
        replacement: "inline-style-prefixer/es/$1",
      },
      {
        // @legendapp/motion's entry re-exports a CJS module with dynamic exports, so esbuild
        // can't see named exports like AnimatePresence (used by gluestack's overlay components).
        // Point at its compiled ESM build (absolute path), whose static `export *` esbuild reads.
        find: /^@legendapp\/motion$/,
        replacement: legendMotionEsm,
      },
    ],
  },

  optimizeDeps: {
    // Force-prebundle the single-file CJS deps react-native-web/gluestack reach so esbuild adds
    // ESM default-export interop (the rnw flow/jsx loader otherwise drops their `default`).
    // (inline-style-prefixer is multi-file with dynamic plugin loads, handled via the es/ alias
    // above instead of here.)
    include: [
      "tailwindcss/resolveConfig",
      "@react-native/normalize-colors",
      "fbjs",
      "memoize-one",
      "nullthrows",
      "postcss-value-parser",
      "styleq",
      "styleq/transform-localize-style",
    ],
  },

  // Tauri-tailored settings (unchanged):
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
