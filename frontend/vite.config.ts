import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  server: {
    // The Vite dev server proxies API calls to the local model-serving
    // daemon; the real management/gateway endpoints are added in later
    // jobs (see docs/architecture.md §1).
    proxy: {
      '/healthz': 'http://127.0.0.1:8137',
    },
  },
})