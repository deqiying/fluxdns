import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": "/src",
    },
  },
  server: {
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8080",
        changeOrigin: false,
      },
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // mock worker 只服务本地 fixture，生产产物不得携带可拦截请求的 worker。
    copyPublicDir: false,
  },
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    clearMocks: true,
    restoreMocks: true,
    css: true,
    // 全应用渲染加 MSW 的用例在 2 vCPU 的 CI runner 上比本地慢一倍以上，默认 5000ms 会偶发超时，这里放宽到 15000ms。
    testTimeout: 15_000,
  },
});
