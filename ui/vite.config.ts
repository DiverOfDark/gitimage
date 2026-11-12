import { svelte } from '@sveltejs/vite-plugin-svelte';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';
import { mockApi } from './mock/plugin';

// `npm run dev` serves the app against mock data (no cluster needed).
// `GITIMAGE_UI_PROXY=http://localhost:8082 npm run dev` proxies to a real controller instead.
const proxy = process.env.GITIMAGE_UI_PROXY;

export default defineConfig({
  plugins: [tailwindcss(), svelte(), ...(proxy ? [] : [mockApi()])],
  server: proxy ? { proxy: { '/api': proxy, '/auth': proxy } } : {},
  build: { outDir: 'dist', emptyOutDir: true, sourcemap: false },
});
