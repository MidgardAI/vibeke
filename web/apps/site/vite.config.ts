import { defineConfig } from 'vite'
import { tanstackStart } from '@tanstack/react-start/plugin/vite'
import tailwindcss from '@tailwindcss/vite'
import { nitro } from 'nitro/vite'
import react from '@vitejs/plugin-react'
import { docsPlugin } from './content/docs-plugin.ts'
import { docManifest } from './content/manifest.ts'

// The crawler treats `/docs/cli#vibeke-task`, `/docs/cli` and `/docs` vs `/docs/` as separate
// pages, but they all write the same output file. Prerendered concurrently, one task can fetch
// the static file while another is still writing it and then write that partial copy back, which
// left `/docs/cli/index.html` truncated on CI. Prerender each output path once.
const prerenderedPaths = new Set<string>()
function firstForOutputPath(path: string) {
  const key = path.split(/[?#]/)[0]!.replace(/\/+$/, '') || '/'
  if (prerenderedPaths.has(key)) return false
  prerenderedPaths.add(key)
  return true
}

export default defineConfig({
  plugins: [
    docsPlugin(),
    tailwindcss(),
    tanstackStart({
      prerender: { enabled: true, crawlLinks: true, failOnError: true, filter: page => firstForOutputPath(page.path) },
      pages: docManifest.map(doc => ({ path: `/docs/${doc.slug}` })),
    }),
    nitro({
      routeRules: {
        '/**': {
          headers: {
            'x-frame-options': 'DENY',
            'content-security-policy': "frame-ancestors 'none'",
            'x-content-type-options': 'nosniff',
            'referrer-policy': 'strict-origin-when-cross-origin',
          },
        },
        '/install.sh': {
          redirect: { to: 'https://github.com/MidgardAI/vibeke/releases/latest/download/install.sh', status: 302 },
          headers: { 'cache-control': 'no-store' },
        },
      },
    }),
    react(),
  ],
})
