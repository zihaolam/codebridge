import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Build output goes into the Go binary via //go:embed (internal/web/dist).
// emptyOutDir is off so the committed .gitkeep survives; the npm build script
// clears assets/ first so stale hashed bundles don't accumulate.
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: '../internal/web/dist',
    emptyOutDir: false,
  },
  server: {
    // `npm run dev` proxies the WS to a locally running `cb web` bridge.
    proxy: {
      '/ws': { target: 'ws://127.0.0.1:8899', ws: true },
    },
  },
})
