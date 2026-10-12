import { createRootRoute, HeadContent, Outlet, Scripts } from '@tanstack/react-router'
import posthog from 'posthog-js'
import { useEffect } from 'react'
import { Footer, Header, NotFound } from '../components/shell'
import stylesheet from '../styles.css?url'

// Google Ads tag for conversion tracking.
const googleAdsId = 'AW-18508808683'

// PostHog product analytics, started in the browser only.
const posthogKey = 'phc_yJDq6yUfcago7PRT9RRFWFatZzoGNvKPjpzX2Rz5wgc2'

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      { name: 'viewport', content: 'width=device-width, initial-scale=1' },
      { title: 'Vibeke — Terminal workspaces for coding agents' },
      { name: 'description', content: 'Run coding agents in a terminal workspace. Answer requests from the inbox. Use separate worktrees and remote hosts.' },
      { name: 'theme-color', content: '#111311' },
      { property: 'og:type', content: 'website' },
      { property: 'og:site_name', content: 'Vibeke' },
      { property: 'og:image', content: 'https://vibeke.dev/brand/social.png' },
      { property: 'og:image:width', content: '1200' },
      { property: 'og:image:height', content: '630' },
      { property: 'og:image:alt', content: 'Vibeke. Run your agents. Keep control. A Muscovy duck in a terminal window.' },
      { name: 'twitter:card', content: 'summary_large_image' },
      { name: 'twitter:image', content: 'https://vibeke.dev/brand/social.png' },
      { name: 'twitter:image:alt', content: 'Vibeke. Run your agents. Keep control. A Muscovy duck in a terminal window.' },
    ],
    links: [
      { rel: 'stylesheet', href: stylesheet },
      { rel: 'icon', type: 'image/x-icon', href: '/favicon.ico' },
      { rel: 'icon', type: 'image/png', sizes: '16x16', href: '/brand/favicon-16.png' },
      { rel: 'icon', type: 'image/png', sizes: '32x32', href: '/brand/favicon-32.png' },
      { rel: 'icon', type: 'image/png', sizes: '48x48', href: '/brand/favicon-48.png' },
      { rel: 'apple-touch-icon', sizes: '180x180', href: '/apple-touch-icon.png' },
      { rel: 'manifest', href: '/site.webmanifest' },
    ],
    scripts: [
      { src: `https://www.googletagmanager.com/gtag/js?id=${googleAdsId}`, async: true },
      { children: `window.dataLayer=window.dataLayer||[];function gtag(){dataLayer.push(arguments);}gtag('js',new Date());gtag('config','${googleAdsId}');` },
    ],
  }),
  component: Root,
  notFoundComponent: NotFound,
})

function Root() {
  useEffect(() => { posthog.init(posthogKey, { api_host: 'https://eu.i.posthog.com', defaults: '2026-05-30' }) }, [])
  return <html lang="en"><head><HeadContent /></head><body><a href="#main" className="skip-link">Skip to content</a><Header /><Outlet /><Footer /><Scripts /></body></html>
}
