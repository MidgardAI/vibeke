import { createRootRoute, HeadContent, Outlet, Scripts } from '@tanstack/react-router'
import { Footer, Header, NotFound } from '../components/shell'
import stylesheet from '../styles.css?url'

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
  }),
  component: Root,
  notFoundComponent: NotFound,
})

function Root() {
  return <html lang="en"><head><HeadContent /></head><body><a href="#main" className="skip-link">Skip to content</a><Header /><Outlet /><Footer /><Scripts /></body></html>
}
