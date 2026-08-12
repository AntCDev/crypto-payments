import { defineConfig } from 'vite';
import tailwindcss from '@tailwindcss/vite';
import { resolve } from 'path';

export default defineConfig({
  plugins: [
    tailwindcss(),
  ],
  build: {
    outDir: '../wwwroot',
    emptyOutDir: true,
    rollupOptions: {
      input: {
        main: resolve(__dirname, 'index.html'),
        invoices: resolve(__dirname, 'invoices.html'),
        // Emitted as wwwroot/checkout/SOL.html, which is exactly the path
        // checkout_views.path points at for the Solana handlers.
        checkoutSol: resolve(__dirname, 'checkout/SOL.html'),
      },
    },
  },
  // @solana/spl-token reaches for Node's Buffer. Aliasing it to the browser
  // shim is cheaper than a full polyfill plugin.
  resolve: {
    alias: {
      buffer: 'buffer/',
    },
  },
  optimizeDeps: {
    include: ['buffer'],
  },
  define: {
    global: 'globalThis',
  },
  server: {
    proxy: {
      // Point at your axum server so /api and /invoice work in `vite dev`.
      '/api': 'http://localhost:8080',
    },
  },
});
