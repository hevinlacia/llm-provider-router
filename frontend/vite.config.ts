import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  // 部署基路径：VITE_BASE=/llm-router/ 构建时资源与路由带前缀；默认 '/' 行为不变
  base: process.env.VITE_BASE || '/',
  plugins: [react()],
  server: {
    proxy: {
      '/api': 'http://127.0.0.1:8789',
      '/v1': 'http://127.0.0.1:8789',
      '/health': 'http://127.0.0.1:8789',
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
});
