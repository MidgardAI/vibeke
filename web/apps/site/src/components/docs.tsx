import { Link } from '@tanstack/react-router'
import { ArrowLeft, ArrowRight, ArrowUpRight, BookOpen, ChevronDown } from 'lucide-react'
import { Children, isValidElement, useState } from 'react'
import type { ReactNode } from 'react'
import Markdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import rehypeSlug from 'rehype-slug'
import rehypeHighlight from 'rehype-highlight'
import { docs } from 'virtual:vibeke-docs'
import type { Doc } from '../../content/manifest'
import { ApiReference } from './api-reference'
import { CopyButton, Eyebrow } from './ui'
import { latestRelease } from '../lib/release'

export function DocsNav({ current }: { current?: string }) {
  const groups = [...new Set(docs.map(doc => doc.group))]
  return <nav aria-label="Documentation chapters"><Link to="/docs" className={`mb-7 flex items-center gap-2 text-xs ${current ? 'text-muted hover:text-cream' : 'text-accent'}`}><BookOpen size={14} /> Documentation</Link>{groups.map(group => <div key={group} className="mb-7"><p className="mb-3 font-mono text-[10px] uppercase tracking-widest text-muted">{group}</p><ul className="space-y-0.5">{docs.filter(doc => doc.group === group).map(doc => <li key={doc.slug}><Link to="/docs/$slug" params={{ slug: doc.slug }} aria-current={current === doc.slug ? 'page' : undefined} className={`-ml-3 block border-l px-3 py-2 text-xs ${current === doc.slug ? 'border-accent bg-accent/5 text-accent' : 'border-transparent text-muted hover:bg-panel hover:text-cream'}`}>{doc.title}</Link></li>)}</ul></div>)}</nav>
}

export function DocsLayout({ current, children, headings = [] }: { current?: string; children: ReactNode; headings?: Doc['headings'] }) {
  const [mobileOpen, setMobileOpen] = useState(false)
  return <main id="main" className="page-width">
    <div className="border-b border-line py-4 lg:hidden"><button type="button" aria-expanded={mobileOpen} aria-controls="docs-mobile-menu" className="flex w-full items-center justify-between text-xs text-muted" onClick={() => setMobileOpen(!mobileOpen)}><span className="flex items-center gap-2"><BookOpen size={15} />Documentation menu</span><ChevronDown size={15} /></button>{mobileOpen && <div id="docs-mobile-menu" className="pt-7" onClick={event => { if ((event.target as HTMLElement).closest('a')) setMobileOpen(false) }}><DocsNav {...(current ? { current } : {})} /></div>}</div>
    <div className="grid min-w-0 gap-12 lg:grid-cols-[185px_minmax(0,1fr)] xl:grid-cols-[185px_minmax(0,1fr)_150px] xl:gap-12"><aside className="sticky top-[108px] hidden max-h-[calc(100vh-132px)] overflow-y-auto self-start pr-2 pb-6 lg:mt-10 lg:block"><DocsNav {...(current ? { current } : {})} /></aside><div className="min-w-0 py-10 sm:py-12">{children}</div><aside className="sticky top-[120px] mt-12 hidden max-h-[calc(100vh-150px)] overflow-y-auto self-start pb-6 xl:block">{headings.length > 0 && <nav aria-label="On this page"><Eyebrow>On this page</Eyebrow><ul className="mt-4 space-y-3">{headings.map(heading => <li key={heading.id}><a href={`#${heading.id}`} className={`block text-[11px] leading-5 text-muted hover:text-accent ${heading.depth === 3 ? 'pl-3' : ''}`}>{heading.text}</a></li>)}</ul></nav>}<div className="mt-8 border-t border-line pt-5 font-mono text-[10px] leading-5 text-muted"><a href={latestRelease.url} className="text-green hover:text-accent">v{latestRelease.version}</a></div></aside></div>
  </main>
}

function textContent(children: ReactNode): string {
  return Children.toArray(children).map(child => isValidElement<{ children?: ReactNode }>(child) ? textContent(child.props.children) : typeof child === 'string' || typeof child === 'number' ? String(child) : '').join('')
}

export function DocArticle({ doc }: { doc: Doc }) {
  const index = docs.findIndex(item => item.slug === doc.slug)
  const previous = docs[index - 1]
  const next = docs[index + 1]
  return <DocsLayout key={doc.slug} current={doc.slug} headings={doc.headings}>
    <div className="mb-4 flex items-center gap-2 font-mono text-[10px] text-muted"><Link to="/docs" className="hover:text-accent">docs</Link><span>/</span><span>{doc.group.toLowerCase()}</span></div>
    <div className="flex items-start justify-between gap-3"><h1 className="text-4xl leading-tight font-medium tracking-[-0.045em] sm:text-[42px]">{doc.title}</h1><CopyButton text={`# ${doc.title}\n\n${doc.body}`} label="Copy page as Markdown" /></div><p className="mt-4 text-base leading-7 text-muted">{doc.description}</p>
    <article className="doc-prose mt-8"><Markdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeSlug, [rehypeHighlight, { detect: false }]]} components={{
      pre: ({ children }) => <div className="relative my-6 overflow-hidden border border-line bg-panel"><CopyButton className="absolute top-2 right-2 bg-panel" text={textContent(children).replace(/\n$/, '')} label="Copy code" /><pre>{children}</pre></div>,
      table: ({ children }) => <div className="my-6 overflow-x-auto border border-line" tabIndex={0} role="region" aria-label="Reference table"><table>{children}</table></div>,
      a: ({ href, children }) => href?.startsWith('/docs/') && !href.includes('#') ? <Link to="/docs/$slug" params={{ slug: href.slice('/docs/'.length) }}>{children}</Link> : <a href={href}>{children}</a>,
    }}>{doc.slug === 'api' ? doc.body.split('## Methods')[0] : doc.body}</Markdown>{doc.slug === 'api' && <ApiReference />}</article>
    <div className="mt-12 flex flex-wrap items-center justify-between gap-4 border-t border-line pt-6 text-[11px] text-muted"><a href={`https://github.com/MidgardAI/vibeke/blob/main/docs/site/src/${doc.file}`} className="inline-flex items-center gap-1 hover:text-accent">View page source <ArrowUpRight size={12} /></a></div>
    <nav aria-label="Previous and next pages" className="mt-8 grid grid-cols-2 gap-4">{previous ? <Link to="/docs/$slug" params={{ slug: previous.slug }} className="border border-line p-4 hover:border-muted"><span className="flex items-center gap-1 font-mono text-[9px] text-muted"><ArrowLeft size={11} /> PREVIOUS</span><span className="mt-2 block text-xs">{previous.title}</span></Link> : <span />}{next && <Link to="/docs/$slug" params={{ slug: next.slug }} className="border border-line p-4 text-right hover:border-muted"><span className="flex items-center justify-end gap-1 font-mono text-[9px] text-muted">NEXT <ArrowRight size={11} /></span><span className="mt-2 block text-xs">{next.title}</span></Link>}</nav>
  </DocsLayout>
}
