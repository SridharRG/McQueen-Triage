import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// In dev, the Rust server runs on :3028 and serves /api; Vite serves the UI.
export default defineConfig({
  plugins: [react()],
  server: {
    port: 5173,
    proxy: { '/api': { target: 'http://localhost:3028', changeOrigin: true } },
  },
  build: { outDir: 'dist', chunkSizeWarningLimit: 4000, sourcemap: false },
});
