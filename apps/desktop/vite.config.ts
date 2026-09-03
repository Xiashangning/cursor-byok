import { codeInspectorPlugin } from "code-inspector-plugin";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { staticI18nPlugin } from "./plugins/static-i18n-plugin.ts";

const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(async ({ command }) => ({
  base: "/__byok-api__/",
  plugins: [
    staticI18nPlugin(),
    ...(command === "serve"
      ? [codeInspectorPlugin({
          bundler: "vite",
          editor: "cursor",
          hotKeys: ["ctrlKey"],
          pathType: "absolute",
          ...(process.platform === "darwin" ? { launchType: "open" as const } : {}),
        })]
      : []),
    react(),
  ],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  build: {
    rolldownOptions: {
      output: {
        codeSplitting: {
          groups: [
            {
              name: "echarts",
              test: /node_modules[\\/](echarts|zrender)\//,
              priority: 20,
            },
            {
              name: "chartjs",
              test: /node_modules[\\/](chart\.js|react-chartjs-2)\//,
              priority: 20,
            },
            {
              name: "react-vendor",
              test: /node_modules[\\/](react|react-dom|scheduler)\//,
              priority: 10,
            },
          ],
        },
      },
    },
    // monaco-editor chunks are lazy-loaded and inherently large (editor.api ~2.6 MB);
    // keep the warning meaningful for everything else.
    chunkSizeWarningLimit: 2700,
  },
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || "127.0.0.1",
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
