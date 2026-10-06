import { defineConfig } from 'vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'
import tailwindcss from '@tailwindcss/vite'
import { nitro } from 'nitro/vite'
import react from '@vitejs/plugin-react'
import { docsPlugin } from './content/docs-plugin.ts'
import { docManifest } from './content/manifest.ts'

export default defineConfig({
  plugins: [
    docsPlugin(),
    tailwindcss(),
    tanstackStart({
      prerender: { enabled: true, crawlLinks: true, failOnError: true },
      pages: docManifest.map(doc => ({ path: `/docs/${doc.slug}` })),
    }),
    nitro(),
    react(),
  ],
})
