export const siteOrigin = 'https://vibeke.dev'
export const siteTitle = 'Vibeke — Run agents in your terminal. Answer from your phone.'
export const siteDescription = 'Run Claude Code, Codex, and other agent CLIs on your laptop or an SSH host. Pair your phone to answer supported requests.'
export const socialImage = `${siteOrigin}/brand/workflow-social.png`

export function canonicalUrl(path: string) {
  const pathname = path.split(/[?#]/)[0]!.replace(/\/+$/, '')
  return `${siteOrigin}${pathname || '/'}`
}

export function jsonLd(value: unknown) {
  return { type: 'application/ld+json', children: JSON.stringify(value).replace(/</g, '\\u003c') }
}

export function pageHead({ path, title, description, doc = false }: { path: string; title: string; description: string; doc?: boolean }) {
  const url = canonicalUrl(path)
  const breadcrumbs = [
    { name: 'Vibeke', item: `${siteOrigin}/` },
    ...(doc ? [{ name: 'Documentation', item: `${siteOrigin}/docs` }] : []),
    ...(path === '/' ? [] : [{ name: doc ? title.replace(/ — Vibeke Docs$/, '') : title.replace(/ — Vibeke$/, ''), item: url }]),
  ]
  return {
    meta: [
      { title },
      { name: 'description', content: description },
      { name: 'robots', content: 'index, follow, max-image-preview:large, max-snippet:-1, max-video-preview:-1' },
      { property: 'og:title', content: title },
      { property: 'og:description', content: description },
      { property: 'og:url', content: url },
      { name: 'twitter:title', content: title },
      { name: 'twitter:description', content: description },
    ],
    links: [{ rel: 'canonical', href: url }],
    scripts: [jsonLd({
      '@context': 'https://schema.org',
      '@graph': [
        { '@type': doc ? 'TechArticle' : 'WebPage', '@id': `${url}#page`, url, name: title, ...(doc ? { headline: title.replace(/ — Vibeke Docs$/, '') } : {}), description, inLanguage: 'en', isPartOf: { '@id': `${siteOrigin}/#website` }, image: { '@type': 'ImageObject', url: socialImage, width: 1200, height: 630 } },
        ...(path === '/' ? [
          { '@type': 'WebSite', '@id': `${siteOrigin}/#website`, url: `${siteOrigin}/`, name: 'Vibeke', inLanguage: 'en' },
        ] : [{ '@type': 'BreadcrumbList', itemListElement: breadcrumbs.map((item, index) => ({ '@type': 'ListItem', position: index + 1, ...item })) }]),
      ],
    })],
  }
}

export function notFoundHead() {
  return { meta: [{ title: 'Page not found — Vibeke' }, { name: 'robots', content: 'noindex' }] }
}
