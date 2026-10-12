import { createRootRoute, HeadContent, Outlet, Scripts } from '@tanstack/react-router'
import posthog from 'posthog-js'
import { useEffect } from 'react'
import { Footer, Header, NotFound } from '../components/shell'
import stylesheet from '../styles.css?url'
import { notFoundHead, socialImage } from '../lib/seo'

// Google Ads tag for conversion tracking.
const googleAdsId = 'AW-18508808683'

// PostHog product analytics, started in the browser only.
const posthogKey = 'phc_yJDq6yUfcago7PRT9RRFWFatZzoGNvKPjpzX2Rz5wgc2'

export const Route = createRootRoute({
  head: () => ({
    meta: [
      { charSet: 'utf-8' },
      { name: 'viewport', content: 'width=device-width, initial-scale=1' },
      // Known pages override these defaults through pageHead; unmatched routes stay noindex.
      ...notFoundHead().meta,
      { name: 'theme-color', content: '#111311' },
      { property: 'og:type', content: 'website' },
      { property: 'og:site_name', content: 'Vibeke' },
      { property: 'og:locale', content: 'en_US' },
      { property: 'og:image', content: socialImage },
      { property: 'og:image:type', content: 'image/png' },
      { property: 'og:image:width', content: '1200' },
      { property: 'og:image:height', content: '630' },
      { property: 'og:image:alt', content: 'Vibeke. Run agents in your terminal. Answer from your phone. Terminal and phone screenshots.' },
      { name: 'twitter:card', content: 'summary_large_image' },
      { name: 'twitter:image', content: socialImage },
      { name: 'twitter:image:alt', content: 'Vibeke. Run agents in your terminal. Answer from your phone. Terminal and phone screenshots.' },
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
      { children: `if(!navigator.webdriver){var ads=document.createElement('script');ads.async=true;ads.src='https://www.googletagmanager.com/gtag/js?id=${googleAdsId}';document.head.appendChild(ads);window.dataLayer=window.dataLayer||[];function gtag(){dataLayer.push(arguments);}gtag('js',new Date());gtag('config','${googleAdsId}');}` },
    ],
  }),
  component: Root,
  notFoundComponent: NotFound,
})

function Root() {
  useEffect(() => { if (navigator.webdriver) return; posthog.init(posthogKey, { api_host: 'https://eu.i.posthog.com', defaults: '2026-05-30' }) }, [])
  return <html lang="en"><head><HeadContent /></head><body><a href="#main" className="skip-link">Skip to content</a><Header /><Outlet /><Footer /><Scripts /></body></html>
}
