import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 개발/빌드 연동. 프론트는 플레인 브라우저에서도 렌더되도록 작성(디자인 검증용).
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  // Tauri는 고정 포트를 기대한다(devUrl과 일치).
  clearScreen: false,
  server: {
    host: host || "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
  // 상대 경로 자산: Tauri asset 프로토콜과 브라우저 file 로딩 양쪽 호환.
  build: {
    target: "es2021",
    sourcemap: false,
  },
});
