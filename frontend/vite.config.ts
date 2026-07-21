import { defineConfig } from 'vite';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  plugins: [
    tailwindcss(),
  ],
  build: {
    // Output directly into Rust's wwwroot folder
    outDir: '../wwwroot',
    // Clear old files in wwwroot before building
    emptyOutDir: true, 
  },
});