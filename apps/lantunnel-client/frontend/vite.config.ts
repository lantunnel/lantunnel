import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

export default defineConfig({
  plugins: [react()],
  // The phones load this bundle off disk, at file:///android_asset/ui/ and at
  // the extension bundle's own directory. An absolute /assets/... path resolves
  // to the filesystem root there and the page comes up blank.
  base: './',
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: '127.0.0.1',
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: 'esnext',
    // Vite 8 minifies with oxc. Naming esbuild here would pull the separate,
    // now-deprecated esbuild package back in — the dependency this upgrade
    // exists to drop.
    minify: 'oxc',
    sourcemap: false,
  },
})
